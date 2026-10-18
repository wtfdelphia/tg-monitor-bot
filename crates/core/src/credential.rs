//! 凭据密文的加解密。定义处是 docs/design/spec/05-安全与租户隔离.md §七：
//! 每租户一个 DEK，`identity_secrets.secret_ciphertext` 用该 DEK 做 AES-256-GCM。
//!
//! 本模块只做纯计算，不碰库、不碰 KEK —— DEK 怎么来、怎么包裹在 `tgm-db` 的
//! `credentials` 里。拆开的理由与 `config.rs` 相同：纯函数的断言不需要容器。
//!
//! **AAD 绑定身份**（spec/05 §七 没写，这里补上）：密文的附加认证数据是
//! `tgm:identity_secrets:secret:v1:` 加上 identity id 的 8 字节大端编码。
//! 不绑的话，能写 `identity_secrets` 的人（DBA、或绕过应用层的一条 UPDATE）
//! 可以把身份 A 的密文和 nonce 整行抄给同租户的身份 B，解密照样成功 ——
//! B 就静默地拿到了 A 的凭据。绑定之后这种搬运在解密时直接失败。

use crate::kek::{DEK_LEN, KekError};
use crate::secret::Secret;
use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};

/// AES-GCM 的 nonce 长度，对应 `identity_secrets.nonce` 列。
pub const NONCE_LEN: usize = 12;

const SECRET_AAD_PREFIX: &[u8] = b"tgm:identity_secrets:secret:v1:";

/// 一次加密的产物，两个字段分别落 `secret_ciphertext` 与 `nonce`。
#[derive(Debug)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// 新生成一个 DEK。来源是系统 CSPRNG。
pub fn generate_dek() -> Result<Secret<[u8; DEK_LEN]>, KekError> {
    let key = Key::<Aes256Gcm>::try_generate().map_err(|_| KekError::Rng)?;
    let mut raw: [u8; DEK_LEN] = key.into();
    let dek = Secret::new(raw);
    zeroize::Zeroize::zeroize(&mut raw);
    Ok(dek)
}

fn secret_aad(identity_id: i64) -> Vec<u8> {
    let mut aad = SECRET_AAD_PREFIX.to_vec();
    aad.extend_from_slice(&identity_id.to_be_bytes());
    aad
}

/// 加密一个身份的凭据明文。nonce 每次新生成：同一 DEK 下重用 nonce
/// 会泄漏两条明文的异或差，而一个租户的所有身份共用一个 DEK。
pub fn seal_identity_secret(
    dek: &Secret<[u8; DEK_LEN]>,
    identity_id: i64,
    plaintext: &[u8],
) -> Result<Sealed, KekError> {
    let nonce: Nonce<_> = Nonce::try_generate().map_err(|_| KekError::Rng)?;
    let aad = secret_aad(identity_id);
    let ciphertext = Aes256Gcm::new(dek.expose().into())
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| KekError::Crypto)?;
    Ok(Sealed {
        ciphertext,
        nonce: nonce.to_vec(),
    })
}

/// 解密。任何失败（nonce 长度、tag、AAD 不符）都只报 `Crypto`，
/// 不给调用方区分原因的 oracle（同 `KekError` 头部注释）。
pub fn open_identity_secret(
    dek: &Secret<[u8; DEK_LEN]>,
    identity_id: i64,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Secret<Vec<u8>>, KekError> {
    let nonce: &Nonce<_> = nonce.try_into().map_err(|_| KekError::Crypto)?;
    let aad = secret_aad(identity_id);
    let plain = Aes256Gcm::new(dek.expose().into())
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| KekError::Crypto)?;
    Ok(Secret::new(plain))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{NONCE_LEN, generate_dek, open_identity_secret, seal_identity_secret};

    #[test]
    fn roundtrip_and_fresh_nonce() {
        let dek = generate_dek().expect("生成 DEK 失败");
        let a = seal_identity_secret(&dek, 7, b"bot-token").expect("加密失败");
        let b = seal_identity_secret(&dek, 7, b"bot-token").expect("加密失败");
        assert_eq!(a.nonce.len(), NONCE_LEN);
        assert_ne!(a.nonce, b.nonce, "nonce 重用了");
        assert_ne!(a.ciphertext, b.ciphertext);
        let back = open_identity_secret(&dek, 7, &a.nonce, &a.ciphertext).expect("解密失败");
        assert_eq!(back.expose().as_slice(), b"bot-token");
    }

    #[test]
    fn ciphertext_is_bound_to_identity() {
        let dek = generate_dek().expect("生成 DEK 失败");
        let sealed = seal_identity_secret(&dek, 7, b"bot-token").expect("加密失败");
        assert!(
            open_identity_secret(&dek, 8, &sealed.nonce, &sealed.ciphertext).is_err(),
            "密文搬到别的身份上竟然解开了"
        );
    }

    #[test]
    fn rejects_wrong_dek_tampering_and_bad_nonce() {
        let dek = generate_dek().expect("生成 DEK 失败");
        let other = generate_dek().expect("生成 DEK 失败");
        assert_ne!(
            dek.expose(),
            other.expose(),
            "两个 DEK 相同，随机数源有问题"
        );
        let sealed = seal_identity_secret(&dek, 7, b"bot-token").expect("加密失败");
        assert!(open_identity_secret(&other, 7, &sealed.nonce, &sealed.ciphertext).is_err());
        let mut tampered = sealed.ciphertext.clone();
        tampered[0] ^= 1;
        assert!(open_identity_secret(&dek, 7, &sealed.nonce, &tampered).is_err());
        assert!(open_identity_secret(&dek, 7, &sealed.nonce[..8], &sealed.ciphertext).is_err());
    }
}
