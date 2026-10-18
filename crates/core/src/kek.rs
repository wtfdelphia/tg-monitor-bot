//! KEK 的 trait 与本地实现。定义处是 docs/design/eng/01-配置与Secret.md §三，
//! 决策是 ADR-0011（信封加密）与 ADR-0012（第一天就抽 trait）。
//!
//! ADR-0012 将 trait 与本地 `EnvKek` 分开，使开发环境和 CI 不依赖真实 KMS。
//! 生产 `KmsKek` 位于 `bins/tgm`，AWS SDK 不进入 `core` 的依赖树。

use crate::profile::Profile;
use crate::secret::Secret;
use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

/// DEK 是 AES-256 的密钥，固定 32 字节。
pub const DEK_LEN: usize = 32;
/// AES-GCM 的 nonce 长度。`Aes256Gcm` 的 `NonceSize` 就是 12。
const NONCE_LEN: usize = 12;

/// KEK 操作的失败。
///
/// **刻意不携带任何密钥材料、也不区分「tag 校验失败」与「明文长度不对」之外的细节**：
/// 解密失败的原因对调用方没有可操作性，而细分会给出一个 oracle。
/// 与 eng/00 §三 纪律 3（`sqlx::Error` 不透给客户端）同一条理由。
#[derive(Debug, thiserror::Error)]
pub enum KekError {
    #[error("EnvKek 不得在 production 下使用（ADR-0012：KEK 明文在进程环境里）")]
    ProductionRefused,

    /// 不带 base64 解码器的原始报错，也不带实际长度之外的内容 ——
    /// 报长度是必要的（配错时要能改），报内容则是泄漏。
    #[error("TGM_LOCAL_KEK 必须是 base64 的 {DEK_LEN} 字节，解码后得到 {got} 字节")]
    BadKeyLength { got: usize },

    #[error("TGM_LOCAL_KEK 不是合法的 base64")]
    BadKeyEncoding,

    #[error("系统随机数不可用")]
    Rng,

    /// wrap 与 unwrap 共用这一个。见本枚举的头部注释。
    #[error("KEK 操作失败")]
    Crypto,
}

/// 一次 wrap/unwrap 绑定的加密上下文（ADR-0032）。
///
/// `KmsKek` 把它作为 KMS `EncryptionContext` 发出：CloudTrail 按身份可审计，
/// IAM 可加条件，密文换到别的身份上 `Decrypt` 直接失败。`EnvKek` 用 AAD 实现同样语义。
/// 字段是裸 `i64` 而非 `TenantId`：它只描述密文归属，不授予任何租户上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KekContext {
    tenant_id: i64,
    identity_id: i64,
}

impl KekContext {
    /// 启动期自测专用。业务 id 都由序列生成、从 1 起，0 不会与真实身份冲突。
    pub const SELF_TEST: Self = Self {
        tenant_id: 0,
        identity_id: 0,
    };

    pub fn new(tenant_id: i64, identity_id: i64) -> Self {
        Self {
            tenant_id,
            identity_id,
        }
    }

    /// KMS `EncryptionContext` 的键值对。键名是线上契约，改名会让旧密文解不开。
    pub fn pairs(&self) -> [(&'static str, String); 2] {
        [
            ("tgm:tenant_id", self.tenant_id.to_string()),
            ("tgm:identity_id", self.identity_id.to_string()),
        ]
    }

    fn aad(&self) -> Vec<u8> {
        let mut aad = b"tgm:kek:v1:".to_vec();
        aad.extend_from_slice(&self.tenant_id.to_be_bytes());
        aad.extend_from_slice(&self.identity_id.to_be_bytes());
        aad
    }
}

/// eng/01 §三 的签名。ADR-0032 起 wrap/unwrap 多一个加密上下文参数。
#[async_trait::async_trait]
pub trait Kek: Send + Sync {
    /// 用 KEK 包裹 DEK，产出 `identity_secrets.dek_ciphertext`
    async fn wrap(
        &self,
        dek: &Secret<[u8; DEK_LEN]>,
        context: &KekContext,
    ) -> Result<Vec<u8>, KekError>;
    /// 解开 DEK。上下文必须与 wrap 时一致。返回值用后必须 zeroize（`Secret<T>` 的 `Drop` 已做，见 secret.rs）
    async fn unwrap(
        &self,
        ciphertext: &[u8],
        context: &KekContext,
    ) -> Result<Secret<[u8; DEK_LEN]>, KekError>;
    /// 写入 `identity_secrets.kek_id`，用于将来分辨轮换代次
    fn kek_id(&self) -> &str;
}

/// 本地后端：KEK 明文来自 `TGM_LOCAL_KEK`，用 AES-256-GCM 包裹 DEK。
///
/// 密文布局是 `nonce(12) || ciphertext+tag(48)`，共 60 字节。
/// nonce 与密文同存的理由：GCM 的 nonce 不是秘密，但**必须每次不同** ——
/// 同一 KEK 下重用 nonce 会泄漏异或差，所以每次 wrap 都新生成一个。
///
/// 派生 `Debug` 是安全的、而且是有意的：`key` 的类型是 `Secret<_>`，
/// 派生出来的实现会走它那个恒为 `[REDACTED]` 的 `Debug`。
/// 这正是 eng/00 §五 那条「靠类型不靠人」的形状 —— 一个持有裸 `[u8; 32]`
/// 的版本在这里派生 `Debug` 就会把 KEK 打进日志，而代码看起来一模一样。
#[derive(Debug)]
pub struct EnvKek {
    key: Secret<[u8; DEK_LEN]>,
    kek_id: &'static str,
}

impl EnvKek {
    /// `profile` 必须传进来，不在函数里读环境变量。
    ///
    /// 理由是这条检查要可测：读 env 的版本只能靠改进程环境来测，
    /// 那会和并行测试互相干扰（env 是进程全局的），于是这条最关键的断言
    /// 就会变成一条偶尔跑的测试。参数化之后它是纯函数。
    pub fn new(profile: Profile, local_kek_b64: &str) -> Result<Self, KekError> {
        // 顺序重要：先拒生产，再校验密钥。
        // 反过来的话，一个生产环境里 KEK 配错的部署会先看到「长度不对」，
        // 改对之后才撞上「不许在生产用」—— 两次失败里第一次指错了方向。
        if profile.is_production() {
            return Err(KekError::ProductionRefused);
        }

        let mut raw = B64
            .decode(local_kek_b64.trim())
            .map_err(|_| KekError::BadKeyEncoding)?;
        let key: [u8; DEK_LEN] = raw
            .as_slice()
            .try_into()
            .map_err(|_| KekError::BadKeyLength { got: raw.len() })?;
        // 中间的 Vec 是 base64 解码的产物，它也含 KEK 明文，就地清零。
        // `Secret` 的 Drop 管不到它 —— 那份拷贝在 Secret 之外。
        zeroize::Zeroize::zeroize(&mut raw);

        Ok(Self {
            key: Secret::new(key),
            kek_id: "local:env:v1",
        })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(self.key.expose().into())
    }
}

#[async_trait::async_trait]
impl Kek for EnvKek {
    async fn wrap(
        &self,
        dek: &Secret<[u8; DEK_LEN]>,
        context: &KekContext,
    ) -> Result<Vec<u8>, KekError> {
        let nonce: Nonce<_> = Nonce::try_generate().map_err(|_| KekError::Rng)?;
        let aad = context.aad();
        let mut out = self
            .cipher()
            .encrypt(
                &nonce,
                Payload {
                    msg: dek.expose().as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|_| KekError::Crypto)?;

        let mut framed = Vec::with_capacity(NONCE_LEN + out.len());
        framed.extend_from_slice(&nonce);
        framed.append(&mut out);
        Ok(framed)
    }

    async fn unwrap(
        &self,
        ciphertext: &[u8],
        context: &KekContext,
    ) -> Result<Secret<[u8; DEK_LEN]>, KekError> {
        if ciphertext.len() <= NONCE_LEN {
            return Err(KekError::Crypto);
        }
        let (nonce, body) = ciphertext.split_at(NONCE_LEN);
        // 用 TryFrom 而不是已弃用的 Nonce::from_slice。长度已由上面的
        // `<= NONCE_LEN` 保证，但这里不 unwrap —— 出错也走 Crypto，
        // 不给调用方「长度不对」与「tag 校验失败」的区分（见 KekError 头部）。
        let nonce: &Nonce<_> = nonce.try_into().map_err(|_| KekError::Crypto)?;
        let aad = context.aad();
        let mut plain = self
            .cipher()
            .decrypt(
                nonce,
                Payload {
                    msg: body,
                    aad: &aad,
                },
            )
            .map_err(|_| KekError::Crypto)?;

        let dek: [u8; DEK_LEN] = plain.as_slice().try_into().map_err(|_| KekError::Crypto)?;
        // 同 new() 里那句：解密出来的 Vec 是 DEK 明文的一份拷贝，
        // 移交给 Secret 之后这一份必须清零，否则 zeroize 保护的只是两份里的一份。
        zeroize::Zeroize::zeroize(&mut plain);
        Ok(Secret::new(dek))
    }

    fn kek_id(&self) -> &str {
        self.kek_id
    }
}

/// eng/01 §4.4 的启动期 KEK 往返自测。
///
/// 为什么启动时要做：KMS 的权限问题（key policy 写错、IAM 没绑上）
/// 在不调用时完全看不出来，而第一次真实调用发生在某个租户添加凭据的时候 ——
/// 那时的报错会被当成业务 bug 查。
///
/// 用常量而非随机值，是为了让失败可复现。这个常量不是密钥，
/// 它只经过 wrap/unwrap 一个来回，从不入库。
pub async fn self_test(kek: &dyn Kek) -> Result<(), KekError> {
    const PROBE: [u8; DEK_LEN] = [0x5a; DEK_LEN];

    let ct = kek
        .wrap(&Secret::new(PROBE), &KekContext::SELF_TEST)
        .await?;
    let back = kek.unwrap(&ct, &KekContext::SELF_TEST).await?;
    if back.expose() != &PROBE {
        return Err(KekError::Crypto);
    }
    Ok(())
}

/// control-plane 的启动期自测失败原因（ADR-0032）。
#[derive(Debug, thiserror::Error)]
pub enum EncryptOnlyError {
    #[error(transparent)]
    Kek(#[from] KekError),
    #[error("control-plane 竟然能解开 DEK —— 它的 KMS 权限只允许 kms:Encrypt（ADR-0032）")]
    DecryptAllowed,
}

/// control-plane 专用：wrap 必须成功，unwrap 必须失败。
///
/// 只对 `KmsKek` 有意义 —— `EnvKek` 是对称的，本地必然解得开（ADR-0012 已接受的差距），
/// 所以 `bins/tgm` 只在 `backend=kms` 时调这个，`env` 下只做 wrap。
/// 这让「控制面无解密权」在每次启动时由 IAM 实测，而不是只写在模板里。
pub async fn encrypt_only_self_test(kek: &dyn Kek) -> Result<(), EncryptOnlyError> {
    const PROBE: [u8; DEK_LEN] = [0x5a; DEK_LEN];

    let ct = kek
        .wrap(&Secret::new(PROBE), &KekContext::SELF_TEST)
        .await?;
    if ct.is_empty() {
        return Err(KekError::Crypto.into());
    }
    match kek.unwrap(&ct, &KekContext::SELF_TEST).await {
        Ok(_) => Err(EncryptOnlyError::DecryptAllowed),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
// workspace 的 expect_used / unwrap_used 针对生产路径（eng/00 §三 纪律 3）。
// 测试里它们就是断言本身，换成 `?` 会把失败变成静默的 Err。
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{
        DEK_LEN, EncryptOnlyError, EnvKek, Kek, KekContext, KekError, NONCE_LEN,
        encrypt_only_self_test, self_test,
    };
    use crate::profile::Profile;
    use crate::secret::Secret;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;

    const CTX: KekContext = KekContext {
        tenant_id: 1,
        identity_id: 2,
    };

    fn dev_kek() -> EnvKek {
        EnvKek::new(Profile::Development, &B64.encode([7u8; DEK_LEN]))
            .expect("开发档 + 32 字节 KEK 应当能构造")
    }

    /// ADR-0012 的「这条检查本身要有测试，否则它是句注释」。
    #[test]
    fn env_kek_refuses_production() {
        let err = EnvKek::new(Profile::Production, &B64.encode([7u8; DEK_LEN]))
            .expect_err("EnvKek 在 production 下竟然构造成功了");
        assert!(matches!(err, KekError::ProductionRefused));
    }

    /// 反证上一条不是靠「KEK 恰好也不合法」通过的：同一个 KEK 在开发档下必须成功。
    /// 没有这一条，一个把 KEK 校验写死成永远失败的版本也会让上一条绿。
    #[test]
    fn same_kek_is_accepted_in_development() {
        assert_eq!(dev_kek().kek_id(), "local:env:v1");
    }

    #[test]
    fn rejects_wrong_key_length() {
        let err = EnvKek::new(Profile::Development, &B64.encode([7u8; 16]))
            .expect_err("16 字节的 KEK 应当被拒");
        assert!(matches!(err, KekError::BadKeyLength { got: 16 }), "{err:?}");
    }

    #[test]
    fn rejects_non_base64() {
        let err =
            EnvKek::new(Profile::Development, "这不是 base64").expect_err("非 base64 应当被拒");
        assert!(matches!(err, KekError::BadKeyEncoding), "{err:?}");
    }

    #[tokio::test]
    async fn roundtrip_recovers_the_dek() {
        let kek = dev_kek();
        let dek = [0x11u8; DEK_LEN];
        let ct = kek.wrap(&Secret::new(dek), &CTX).await.expect("wrap 失败");
        assert_eq!(ct.len(), NONCE_LEN + DEK_LEN + 16, "密文布局变了");
        let back = kek.unwrap(&ct, &CTX).await.expect("unwrap 失败");
        assert_eq!(back.expose(), &dek);
    }

    /// nonce 每次新生成。重用 nonce 在同一 KEK 下会泄漏两条明文的异或差，
    /// 而「wrap 两次得到相同密文」是它唯一的外部可观测症状。
    #[tokio::test]
    async fn wrap_is_not_deterministic() {
        let kek = dev_kek();
        let dek = [0x11u8; DEK_LEN];
        let a = kek.wrap(&Secret::new(dek), &CTX).await.expect("wrap 失败");
        let b = kek.wrap(&Secret::new(dek), &CTX).await.expect("wrap 失败");
        assert_ne!(a, b, "同一 DEK 两次 wrap 得到相同密文 —— nonce 被复用了");
        assert_ne!(&a[..NONCE_LEN], &b[..NONCE_LEN], "nonce 段相同");
    }

    #[tokio::test]
    async fn tampered_ciphertext_is_rejected() {
        let kek = dev_kek();
        let mut ct = kek
            .wrap(&Secret::new([0x11u8; DEK_LEN]), &CTX)
            .await
            .expect("wrap 失败");
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(
            kek.unwrap(&ct, &CTX).await.is_err(),
            "改了一位的密文竟然解开了"
        );
    }

    #[tokio::test]
    async fn another_kek_cannot_unwrap() {
        let a = dev_kek();
        let b = EnvKek::new(Profile::Development, &B64.encode([9u8; DEK_LEN])).expect("构造失败");
        let ct = a
            .wrap(&Secret::new([0x11u8; DEK_LEN]), &CTX)
            .await
            .expect("wrap");
        assert!(b.unwrap(&ct, &CTX).await.is_err(), "换了 KEK 也能解开");
    }

    /// ADR-0032：DEK 密文搬到别的租户或别的身份上必须解不开。
    /// 反证：把 aad() 换成空切片，这条红。
    #[tokio::test]
    async fn ciphertext_is_bound_to_context() {
        let kek = dev_kek();
        let ct = kek
            .wrap(&Secret::new([0x11u8; DEK_LEN]), &CTX)
            .await
            .expect("wrap");
        for other in [KekContext::new(1, 3), KekContext::new(2, 2)] {
            assert!(
                kek.unwrap(&ct, &other).await.is_err(),
                "上下文 {other:?} 竟然解开了 {CTX:?} 的 DEK"
            );
        }
        assert!(kek.unwrap(&ct, &CTX).await.is_ok());
    }

    #[test]
    fn context_pairs_are_the_wire_contract() {
        assert_eq!(
            KekContext::new(7, 42).pairs(),
            [
                ("tgm:tenant_id", "7".to_owned()),
                ("tgm:identity_id", "42".to_owned())
            ]
        );
    }

    /// 对称的 EnvKek 必然解得开，所以 encrypt-only 自测对它必须报 DecryptAllowed ——
    /// 这同时证明那条自测不是空转。
    #[tokio::test]
    async fn encrypt_only_self_test_rejects_a_kek_that_can_decrypt() {
        let err = encrypt_only_self_test(&dev_kek())
            .await
            .expect_err("能解密的 KEK 不该通过 encrypt-only 自测");
        assert!(matches!(err, EncryptOnlyError::DecryptAllowed), "{err:?}");
    }

    #[tokio::test]
    async fn encrypt_only_self_test_accepts_an_encrypt_only_kek() {
        struct EncryptOnly;
        #[async_trait::async_trait]
        impl Kek for EncryptOnly {
            async fn wrap(
                &self,
                _dek: &Secret<[u8; DEK_LEN]>,
                _ctx: &KekContext,
            ) -> Result<Vec<u8>, KekError> {
                Ok(vec![1u8; 60])
            }
            async fn unwrap(
                &self,
                _ct: &[u8],
                _ctx: &KekContext,
            ) -> Result<Secret<[u8; DEK_LEN]>, KekError> {
                Err(KekError::Crypto)
            }
            fn kek_id(&self) -> &str {
                "test:encrypt-only"
            }
        }
        encrypt_only_self_test(&EncryptOnly)
            .await
            .expect("只能加密的 KEK 应当通过");
    }

    #[tokio::test]
    async fn self_test_passes_on_a_good_kek() {
        self_test(&dev_kek()).await.expect("往返自测应当通过");
    }

    /// 自测本身不是空转的：给它一个 wrap 能成功但 unwrap 返回别的值的实现，
    /// 它必须报错。没有这一条，一个只 `Ok(())` 的 self_test 也会让上一条绿。
    #[tokio::test]
    async fn self_test_catches_a_broken_kek() {
        struct WrongKek;
        #[async_trait::async_trait]
        impl Kek for WrongKek {
            async fn wrap(
                &self,
                _dek: &Secret<[u8; DEK_LEN]>,
                _ctx: &KekContext,
            ) -> Result<Vec<u8>, KekError> {
                Ok(vec![0u8; 60])
            }
            async fn unwrap(
                &self,
                _ct: &[u8],
                _ctx: &KekContext,
            ) -> Result<Secret<[u8; DEK_LEN]>, KekError> {
                Ok(Secret::new([0u8; DEK_LEN]))
            }
            fn kek_id(&self) -> &str {
                "test:wrong"
            }
        }
        assert!(
            self_test(&WrongKek).await.is_err(),
            "往返自测对一个返回错值的实现没报错 —— 那条启动检查是空的"
        );
    }
}
