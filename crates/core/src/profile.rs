//! 运行画像。取值范围定在 docs/design/eng/01-配置与Secret.md §二（`TGM_PROFILE`）。
//!
//! 做成枚举而不是直接比字符串，是因为有两处**安全检查**以它为条件：
//! §三 的「`EnvKek` 在 production 下拒绝启动」与 §4.5 的日志级别校验。
//! 写成 `profile == "production"` 时，一个 `Production` 或 `prod` 的拼写
//! 会让检查静默放过 —— 失败方向是「不安全且安静」。

/// 解析失败。未知取值**不默认成 development** —— 那会让拼错的生产部署
/// 拿到宽松档。见 [`Profile::parse`]。
#[derive(Debug, thiserror::Error)]
#[error("TGM_PROFILE 只接受 production 或 development，收到 {0:?}")]
pub struct UnknownProfile(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Production,
    Development,
}

impl Profile {
    /// 严格解析，不接受大小写变体、不接受缩写、无默认值。
    pub fn parse(s: &str) -> Result<Self, UnknownProfile> {
        match s {
            "production" => Ok(Self::Production),
            "development" => Ok(Self::Development),
            other => Err(UnknownProfile(other.to_string())),
        }
    }

    pub fn is_production(self) -> bool {
        self == Self::Production
    }
}

#[cfg(test)]
// 同 kek.rs：测试里的 unwrap 就是断言。
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::Profile;

    #[test]
    fn parses_exactly_two_values() {
        assert_eq!(Profile::parse("production").unwrap(), Profile::Production);
        assert_eq!(Profile::parse("development").unwrap(), Profile::Development);
    }

    #[test]
    fn near_misses_are_rejected_not_downgraded() {
        // 这条是本模块存在的理由：下面每一个都**像**生产，
        // 若解析时兜底成 development，EnvKek 就会在生产环境里启动成功。
        for s in [
            "Production",
            "PRODUCTION",
            "prod",
            "prod ",
            " production",
            "",
        ] {
            assert!(Profile::parse(s).is_err(), "{s:?} 被接受了");
        }
    }
}
