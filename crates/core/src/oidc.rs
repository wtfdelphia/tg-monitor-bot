//! OIDC 登录协议层的纯逻辑。
//!
//! 这里只生成和校验协议材料，不负责 HTTP、token exchange、JWKS 请求或会话签发。
//! 这些边界留给控制面实现，避免在尚未批准安全面升级前偷偷改变数据库写入路径。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use getrandom::Error as RandomError;
use sha2::{Digest, Sha256};
use thiserror::Error;

const RANDOM_BYTES: usize = 32;
const CODE_VERIFIER_MIN_LEN: usize = 43;
const CODE_VERIFIER_MAX_LEN: usize = 128;
const LOGIN_STATE_TTL: Duration = Duration::from_secs(600);

/// OIDC 登录协议材料。
///
/// `code_verifier` 只应保留在服务端登录态中，不应返回给浏览器或写入日志。
#[derive(Clone, PartialEq, Eq)]
pub struct OidcLoginMaterial {
    state: String,
    code_verifier: String,
    code_challenge: String,
    nonce: String,
}

impl fmt::Debug for OidcLoginMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcLoginMaterial")
            .field("state", &"[REDACTED]")
            .field("code_verifier", &"[REDACTED]")
            .field("code_challenge", &"[REDACTED]")
            .field("nonce", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum OidcMaterialError {
    #[error("生成 OIDC 登录随机材料失败")]
    Random(#[from] RandomError),
    #[error("生成的 PKCE code_verifier 不符合 RFC 7636 约束")]
    InvalidGeneratedVerifier,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OidcAlgorithmError {
    #[error("OIDC 签名算法配置不受支持")]
    Unsupported,
    #[error("OIDC id_token 签名算法与服务端配置不一致")]
    Mismatch,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OidcTokenHeaderError {
    #[error("OIDC id_token 不是合法的三段 JWT")]
    InvalidStructure,
    #[error("OIDC id_token header 不是合法的 base64url")]
    InvalidEncoding,
    #[error("OIDC id_token header 不是合法的 JSON 对象")]
    InvalidJson,
    #[error("OIDC id_token header 缺少 alg")]
    MissingAlgorithm,
    #[error("OIDC id_token header 缺少 kid")]
    MissingKeyId,
    #[error("OIDC id_token header 算法校验失败")]
    Algorithm(OidcAlgorithmError),
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OidcVerifierError {
    #[error("PKCE code_verifier 不符合 RFC 7636 长度或字符约束")]
    InvalidCodeVerifier,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OidcNonceError {
    #[error("OIDC nonce 不匹配")]
    Mismatch,
    #[error("OIDC nonce 已使用或不存在")]
    Replay,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OidcClaimsError {
    #[error("OIDC id_token 的 issuer 不匹配")]
    IssuerMismatch,
    #[error("OIDC id_token 的 audience 不匹配")]
    AudienceMismatch,
    #[error("OIDC id_token 已过期")]
    Expired,
    #[error("OIDC id_token 的 subject 不是正整数 Telegram 用户 id")]
    InvalidSubject,
    #[error("OIDC id_token 的 nonce 校验失败")]
    Nonce(OidcNonceError),
}

/// BotFather 为当前 bot 配置的签名算法。
///
/// 验签时必须使用这个配置值，不能按未经验证的 token header 选择算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcSigningAlgorithm {
    Rs256,
    Es256,
    EdDsa,
    Es256k,
}

impl OidcSigningAlgorithm {
    pub fn parse(value: &str) -> Result<Self, OidcAlgorithmError> {
        match value {
            "RS256" => Ok(Self::Rs256),
            "ES256" => Ok(Self::Es256),
            "EdDSA" => Ok(Self::EdDsa),
            "ES256K" => Ok(Self::Es256k),
            _ => Err(OidcAlgorithmError::Unsupported),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
            Self::Es256k => "ES256K",
        }
    }

    pub fn validate_token_header(self, token_header_alg: &str) -> Result<(), OidcAlgorithmError> {
        if token_header_alg == self.as_str() {
            Ok(())
        } else {
            Err(OidcAlgorithmError::Mismatch)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcTokenHeader {
    algorithm: String,
    key_id: String,
}

impl OidcTokenHeader {
    pub fn parse_and_validate(
        id_token: &str,
        configured_algorithm: OidcSigningAlgorithm,
    ) -> Result<Self, OidcTokenHeaderError> {
        let mut parts = id_token.split('.');
        let encoded_header = parts.next().ok_or(OidcTokenHeaderError::InvalidStructure)?;
        let encoded_payload = parts.next().ok_or(OidcTokenHeaderError::InvalidStructure)?;
        let encoded_signature = parts.next().ok_or(OidcTokenHeaderError::InvalidStructure)?;
        if parts.next().is_some()
            || encoded_header.is_empty()
            || encoded_payload.is_empty()
            || encoded_signature.is_empty()
        {
            return Err(OidcTokenHeaderError::InvalidStructure);
        }

        let header_bytes = URL_SAFE_NO_PAD
            .decode(encoded_header)
            .map_err(|_| OidcTokenHeaderError::InvalidEncoding)?;
        let header = serde_json::from_slice::<serde_json::Value>(&header_bytes)
            .map_err(|_| OidcTokenHeaderError::InvalidJson)?;
        let object = header
            .as_object()
            .ok_or(OidcTokenHeaderError::InvalidJson)?;
        let algorithm = object
            .get("alg")
            .and_then(serde_json::Value::as_str)
            .ok_or(OidcTokenHeaderError::MissingAlgorithm)?;
        let key_id = object
            .get("kid")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(OidcTokenHeaderError::MissingKeyId)?;
        configured_algorithm
            .validate_token_header(algorithm)
            .map_err(OidcTokenHeaderError::Algorithm)?;

        Ok(Self {
            algorithm: algorithm.to_owned(),
            key_id: key_id.to_owned(),
        })
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

impl OidcLoginMaterial {
    pub fn generate() -> Result<Self, OidcMaterialError> {
        let state = random_token()?;
        let code_verifier = random_token()?;
        let code_challenge = code_challenge(&code_verifier)
            .map_err(|_| OidcMaterialError::InvalidGeneratedVerifier)?;
        let nonce = random_token()?;

        Ok(Self {
            state,
            code_verifier,
            code_challenge,
            nonce,
        })
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn code_verifier(&self) -> &str {
        &self.code_verifier
    }

    pub fn code_challenge(&self) -> &str {
        &self.code_challenge
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn state_matches(&self, returned_state: &str) -> bool {
        constant_time_eq(self.state.as_bytes(), returned_state.as_bytes())
    }

    pub fn nonce_matches(&self, token_nonce: &str) -> Result<(), OidcNonceError> {
        if constant_time_eq(self.nonce.as_bytes(), token_nonce.as_bytes()) {
            Ok(())
        } else {
            Err(OidcNonceError::Mismatch)
        }
    }

    pub fn authorization_url(
        &self,
        authorization_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
    ) -> String {
        let mut url = authorization_endpoint.to_owned();
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        append_query_parameter(&mut url, "response_type", "code");
        append_query_parameter(&mut url, "client_id", client_id);
        append_query_parameter(&mut url, "redirect_uri", redirect_uri);
        append_query_parameter(&mut url, "scope", "openid profile");
        append_query_parameter(&mut url, "state", self.state());
        append_query_parameter(&mut url, "code_challenge", self.code_challenge());
        append_query_parameter(&mut url, "code_challenge_method", "S256");
        append_query_parameter(&mut url, "nonce", self.nonce());
        url
    }
}

pub struct OidcIdTokenClaims<'a> {
    issuer: &'a str,
    audiences: &'a [&'a str],
    subject: &'a str,
    nonce: &'a str,
    exp_unix: i64,
}

impl<'a> OidcIdTokenClaims<'a> {
    pub fn new(
        issuer: &'a str,
        audiences: &'a [&'a str],
        subject: &'a str,
        nonce: &'a str,
        exp_unix: i64,
    ) -> Self {
        Self {
            issuer,
            audiences,
            subject,
            nonce,
            exp_unix,
        }
    }
}

pub fn validate_id_token_claims(
    claims: &OidcIdTokenClaims<'_>,
    expected_issuer: &str,
    expected_client_id: &str,
    now_unix: i64,
    material: &OidcLoginMaterial,
) -> Result<i64, OidcClaimsError> {
    if claims.issuer != expected_issuer {
        return Err(OidcClaimsError::IssuerMismatch);
    }
    if !claims.audiences.contains(&expected_client_id) {
        return Err(OidcClaimsError::AudienceMismatch);
    }
    if claims.exp_unix <= now_unix {
        return Err(OidcClaimsError::Expired);
    }
    let subject = claims
        .subject
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(OidcClaimsError::InvalidSubject)?;
    material
        .nonce_matches(claims.nonce)
        .map_err(OidcClaimsError::Nonce)?;
    Ok(subject)
}

pub fn code_challenge(code_verifier: &str) -> Result<String, OidcVerifierError> {
    if !is_valid_code_verifier(code_verifier) {
        return Err(OidcVerifierError::InvalidCodeVerifier);
    }

    let digest = Sha256::digest(code_verifier.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(digest))
}

pub fn is_valid_code_verifier(code_verifier: &str) -> bool {
    let length = code_verifier.len();
    (CODE_VERIFIER_MIN_LEN..=CODE_VERIFIER_MAX_LEN).contains(&length)
        && code_verifier.bytes().all(is_rfc7636_unreserved)
}

fn random_token() -> Result<String, RandomError> {
    let mut bytes = [0_u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn append_query_parameter(url: &mut String, key: &str, value: &str) {
    if !url.ends_with('?') && !url.ends_with('&') {
        url.push('&');
    }
    append_percent_encoded(url, key);
    url.push('=');
    append_percent_encoded(url, value);
}

fn append_percent_encoded(output: &mut String, value: &str) {
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push('%');
            output.push(hex_digit(byte >> 4));
            output.push(hex_digit(byte & 0x0f));
        }
    }
}

const fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => '?',
    }
}

const fn is_rfc7636_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());

    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }

    difference == 0
}

/// 只保存尚未消费的 nonce；消费动作是原子的一次性移除。
#[derive(Default)]
pub struct InMemoryNonceStore {
    nonces: Mutex<HashSet<String>>,
}

impl fmt::Debug for InMemoryNonceStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InMemoryNonceStore([REDACTED])")
    }
}

/// OIDC 起始阶段的短期服务端登录态。
///
/// 这里只给协议骨架使用：正式 callback 接入前，生产环境仍需把它放进有明确
/// 过期与并发语义的服务端存储，不能把 `code_verifier` 放到浏览器可读位置。
// ponytail: 进程内状态，控制面扩容或重启时不共享；批准会话安全面后改成带 TTL 的共享存储
#[derive(Default)]
pub struct OidcLoginStateStore {
    states: Mutex<HashMap<String, (Instant, OidcLoginMaterial)>>,
}

impl fmt::Debug for OidcLoginStateStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OidcLoginStateStore([REDACTED])")
    }
}

impl OidcLoginStateStore {
    pub fn remember(&self, material: OidcLoginMaterial) {
        let mut states = match self.states.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        states.retain(|_, (created_at, _)| now.duration_since(*created_at) < LOGIN_STATE_TTL);
        states.insert(material.state().to_owned(), (now, material));
    }

    pub fn take(&self, state: &str) -> Option<OidcLoginMaterial> {
        let mut states = match self.states.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        states.retain(|_, (created_at, _)| now.duration_since(*created_at) < LOGIN_STATE_TTL);
        states.remove(state).map(|(_, material)| material)
    }

    pub fn take_if_bound(&self, state: &str, browser_state: &str) -> Option<OidcLoginMaterial> {
        if !constant_time_eq(state.as_bytes(), browser_state.as_bytes()) {
            return None;
        }
        self.take(state)
    }
}

impl InMemoryNonceStore {
    pub fn remember(&self, nonce: impl Into<String>) {
        let mut nonces = match self.nonces.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        nonces.insert(nonce.into());
    }

    pub fn consume(&self, nonce: &str) -> Result<(), OidcNonceError> {
        let mut nonces = match self.nonces.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if nonces.remove(nonce) {
            Ok(())
        } else {
            Err(OidcNonceError::Replay)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        InMemoryNonceStore, OidcAlgorithmError, OidcClaimsError, OidcIdTokenClaims,
        OidcLoginMaterial, OidcLoginStateStore, OidcNonceError, OidcSigningAlgorithm,
        OidcTokenHeader, OidcTokenHeaderError, code_challenge, is_valid_code_verifier,
        validate_id_token_claims,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn generated_material_is_valid_pkce_material() {
        let material = OidcLoginMaterial::generate().expect("生成 OIDC 材料失败");

        assert_eq!(material.state().len(), 43);
        assert_eq!(material.nonce().len(), 43);
        assert!(is_valid_code_verifier(material.code_verifier()));
        assert_eq!(
            material.code_challenge(),
            code_challenge(material.code_verifier()).expect("计算 challenge 失败")
        );
        assert!(material.state_matches(material.state()));
        assert!(material.nonce_matches(material.nonce()).is_ok());
    }

    #[test]
    fn debug_output_redacts_login_material_and_nonce_store() {
        let material = OidcLoginMaterial {
            state: "state-secret".to_owned(),
            code_verifier: "verifier-secret".to_owned(),
            code_challenge: "challenge-secret".to_owned(),
            nonce: "nonce-secret".to_owned(),
        };
        let store = InMemoryNonceStore::default();
        store.remember("nonce-secret");

        let material_debug = format!("{material:?}");
        let store_debug = format!("{store:?}");
        for secret in [
            "state-secret",
            "verifier-secret",
            "challenge-secret",
            "nonce-secret",
        ] {
            assert!(!material_debug.contains(secret));
            assert!(!store_debug.contains(secret));
        }
        assert!(material_debug.contains("[REDACTED]"));
        assert!(store_debug.contains("[REDACTED]"));
    }

    #[test]
    fn authorization_url_contains_encoded_protocol_parameters() {
        let material = OidcLoginMaterial {
            state: "state-value".to_owned(),
            code_verifier: "a".repeat(43),
            code_challenge: "challenge-value".to_owned(),
            nonce: "nonce-value".to_owned(),
        };

        let url = material.authorization_url(
            "https://oauth.telegram.org/auth?prompt=login",
            "12345",
            "https://console.example/callback?tenant=a&mode=web",
        );

        assert!(url.starts_with("https://oauth.telegram.org/auth?prompt=login&"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope=openid%20profile"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fconsole.example%2Fcallback%3Ftenant%3Da%26mode%3Dweb"
        ));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("nonce=nonce-value"));
    }

    #[test]
    fn rejects_invalid_pkce_verifier() {
        assert!(!is_valid_code_verifier("short"));
        assert!(!is_valid_code_verifier(&"a".repeat(129)));
        assert!(!is_valid_code_verifier(&format!("{}!", "a".repeat(42))));
    }

    #[test]
    fn configured_algorithm_is_not_selected_from_token_header() {
        let configured = OidcSigningAlgorithm::Rs256;

        assert_eq!(OidcSigningAlgorithm::parse("RS256"), Ok(configured));
        assert_eq!(
            OidcSigningAlgorithm::parse("none"),
            Err(OidcAlgorithmError::Unsupported)
        );
        assert_eq!(
            OidcSigningAlgorithm::parse("HS256"),
            Err(OidcAlgorithmError::Unsupported)
        );
        assert!(configured.validate_token_header("RS256").is_ok());
        assert_eq!(
            configured.validate_token_header("HS256"),
            Err(OidcAlgorithmError::Mismatch)
        );
    }

    #[test]
    fn r20_id_token_alg_header_tampering_is_rejected() {
        let configured = OidcSigningAlgorithm::Rs256;

        for tampered_alg in ["none", "HS256"] {
            assert_eq!(
                configured.validate_token_header(tampered_alg),
                Err(OidcAlgorithmError::Mismatch)
            );
        }
    }

    #[test]
    fn r20_rejects_tampered_alg_in_a_real_jwt_header() {
        let configured = OidcSigningAlgorithm::Rs256;
        for tampered_alg in ["none", "HS256"] {
            let header = URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "alg": tampered_alg,
                    "kid": "telegram-key-1"
                }))
                .expect("序列化 JWT header 失败"),
            );
            let token = format!("{header}.eyJzdWIiOiI3MDAxIn0.signature");
            assert_eq!(
                OidcTokenHeader::parse_and_validate(&token, configured),
                Err(OidcTokenHeaderError::Algorithm(
                    OidcAlgorithmError::Mismatch
                ))
            );
        }
    }

    #[test]
    fn jwt_header_requires_kid_and_exactly_three_segments() {
        let header = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({ "alg": "RS256" }))
                .expect("序列化 JWT header 失败"),
        );
        assert_eq!(
            OidcTokenHeader::parse_and_validate(
                &format!("{header}.payload.signature"),
                OidcSigningAlgorithm::Rs256
            ),
            Err(OidcTokenHeaderError::MissingKeyId)
        );
        assert_eq!(
            OidcTokenHeader::parse_and_validate(
                &format!("{header}.payload.signature.extra"),
                OidcSigningAlgorithm::Rs256
            ),
            Err(OidcTokenHeaderError::InvalidStructure)
        );
    }

    #[test]
    fn r21_replayed_nonce_is_rejected() {
        let store = InMemoryNonceStore::default();
        store.remember("nonce-1");

        assert!(store.consume("nonce-1").is_ok());
        assert_eq!(store.consume("nonce-1"), Err(OidcNonceError::Replay));
        assert_eq!(store.consume("nonce-2"), Err(OidcNonceError::Replay));
    }

    #[test]
    fn login_state_is_server_side_and_single_use() {
        let store = OidcLoginStateStore::default();
        let material = OidcLoginMaterial::generate().expect("生成 OIDC 材料失败");
        let state = material.state().to_owned();

        store.remember(material.clone());
        assert_eq!(store.take(&state), Some(material));
        assert!(store.take(&state).is_none());
    }

    #[test]
    fn login_state_cannot_be_consumed_with_a_different_browser_binding() {
        let store = OidcLoginStateStore::default();
        let material = OidcLoginMaterial::generate().expect("生成 OIDC 材料失败");
        let state = material.state().to_owned();
        store.remember(material.clone());

        assert!(store.take_if_bound(&state, "attacker-state").is_none());
        assert_eq!(store.take_if_bound(&state, &state), Some(material));
    }

    #[test]
    fn id_token_claims_require_issuer_audience_expiry_subject_and_nonce() {
        let material = OidcLoginMaterial {
            state: "state".to_owned(),
            code_verifier: "a".repeat(43),
            code_challenge: "challenge".to_owned(),
            nonce: "nonce".to_owned(),
        };
        let audiences = ["12345"];
        let claims = OidcIdTokenClaims::new(
            "https://oauth.telegram.org",
            &audiences,
            "7001",
            "nonce",
            101,
        );

        assert_eq!(
            validate_id_token_claims(
                &claims,
                "https://oauth.telegram.org",
                "12345",
                100,
                &material,
            ),
            Ok(7001)
        );
    }

    #[test]
    fn id_token_claims_reject_wrong_issuer_audience_expiry_subject_and_nonce() {
        let material = OidcLoginMaterial {
            state: "state".to_owned(),
            code_verifier: "a".repeat(43),
            code_challenge: "challenge".to_owned(),
            nonce: "nonce".to_owned(),
        };
        let audiences = ["12345"];
        let cases = [
            (
                OidcIdTokenClaims::new("https://evil.example", &audiences, "7001", "nonce", 101),
                OidcClaimsError::IssuerMismatch,
            ),
            (
                OidcIdTokenClaims::new(
                    "https://oauth.telegram.org",
                    &["99999"],
                    "7001",
                    "nonce",
                    101,
                ),
                OidcClaimsError::AudienceMismatch,
            ),
            (
                OidcIdTokenClaims::new(
                    "https://oauth.telegram.org",
                    &audiences,
                    "7001",
                    "nonce",
                    100,
                ),
                OidcClaimsError::Expired,
            ),
            (
                OidcIdTokenClaims::new(
                    "https://oauth.telegram.org",
                    &audiences,
                    "not-a-user",
                    "nonce",
                    101,
                ),
                OidcClaimsError::InvalidSubject,
            ),
            (
                OidcIdTokenClaims::new(
                    "https://oauth.telegram.org",
                    &audiences,
                    "7001",
                    "replayed",
                    101,
                ),
                OidcClaimsError::Nonce(OidcNonceError::Mismatch),
            ),
        ];

        for (claims, expected) in cases {
            assert_eq!(
                validate_id_token_claims(
                    &claims,
                    "https://oauth.telegram.org",
                    "12345",
                    100,
                    &material,
                ),
                Err(expected)
            );
        }
    }
}
