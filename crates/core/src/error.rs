//! `AppError` 与 11 个错误码的映射。定义处是 eng/00 §三，
//! 错误码表在 spec/07 §2.1（含每个码的 HTTP 状态与场景）。
//!
//! **HTTP 层的 `IntoResponse` impl 不在这里，它随 WP-5 的 axum 落地。**
//! 但映射本身现在就收在本模块：[`AppError::code`]、[`AppError::http_status`]、
//! [`AppError::public_message`] 三个方法。eng/00 §三 纪律 2 要求
//! 「`AppError` → HTTP 的映射只有一处」—— 若等到 WP-5 才开始想这件事，
//! 那时已经有一批 handler 各自按直觉返回状态码了，「只有一处」就再也收不回来。
//! WP-5 的 impl 只许调这三个方法，不许自己 match。
//!
//! 最容易写错的是 `NotFound`：spec/07 §2.1 明确「存在但不属于本租户」
//! 也返回 404 而**不是** 403 —— 403 泄漏「该 id 存在」。
//! 按直觉写就会写成 403，所以下面配了断言。

/// 对外错误。变体顺序与 spec/07 §2.1 的表一致，便于逐条核对。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
    #[error("csrf invalid")]
    CsrfInvalid,
    #[error("not found")]
    NotFound,
    #[error("rule conflict")]
    RuleConflict { existing_id: i64 },
    #[error("condition invalid: {0}")]
    ConditionInvalid(String),
    #[error("regex invalid: {0}")]
    RegexInvalid(String),
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("rate limited")]
    RateLimited,
    #[error("credential via web only")]
    CredentialViaWebOnly,
    #[error("not invited")]
    NotInvited,

    // 以下两个不对外暴露细节，统一 500。
    // `transparent` 是**刻意**的：它让 Display 转发出底层错误全文，
    // 那是给日志看的。对外的文案走 public_message()，见该方法的注释。
    #[error(transparent)]
    Db(#[from] sqlx::Error),

    /// 兜底的 500。**偏离 eng/00 §三 的草图**：那里写的是
    /// `Other(#[from] anyhow::Error)`，但同一节的纪律 1 规定
    /// 「`anyhow` 只用于 `bins`」—— 两句话冲突，这里按纪律走，理由有两条。
    ///
    /// 一是依赖方向：`core` 依赖 `anyhow` 就等于把顶层的类型擦除工具
    /// 装进了库层，纪律 1 那句「库层返回具体错误」失去强制力。
    /// 二更要紧：`#[from] anyhow::Error` 会让库层代码用一个 `?`
    /// 顺手把任何错误擦成 `Other` —— 那正是纪律 1 要防的动作，
    /// 而草图的写法让它变成了最省力的路径。
    ///
    /// 所以这里既不依赖 `anyhow`，也**不给 `#[from]`**：擦除类型必须
    /// 显式写 [`AppError::internal`]，在 diff 里看得见。
    /// `bins` 那层的 `anyhow::Error` 仍然能进来（它实现了
    /// `Into<Box<dyn Error + Send + Sync>>`），只是得自己写出来。
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl AppError {
    /// 包络里的 `code` 字段（spec/07 §2.1 第一列）。
    ///
    /// 这个 `match` 不写 `_ =>` 兜底：加一个变体就编译不过，
    /// 于是「新错误忘了给码」这件事由编译器拦，而不是靠人记得。
    /// 下面 `http_status` 与 `public_message` 同理。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
            Self::CsrfInvalid => "CSRF_INVALID",
            Self::NotFound => "NOT_FOUND",
            Self::RuleConflict { .. } => "RULE_CONFLICT",
            Self::ConditionInvalid(_) => "CONDITION_INVALID",
            Self::RegexInvalid(_) => "REGEX_INVALID",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::RateLimited => "RATE_LIMITED",
            Self::CredentialViaWebOnly => "CREDENTIAL_VIA_WEB_ONLY",
            Self::NotInvited => "NOT_INVITED",
            Self::Db(_) | Self::Other(_) => "INTERNAL",
        }
    }

    /// HTTP 状态码。取值来自 spec/07 §2.1 第二列，不要在别处重新决定。
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            // CsrfInvalid 与 Forbidden 同为 403，但 code 不同 —— 前端要能区分
            // 「重新登录」和「刷新页面拿新 token」。
            Self::Forbidden | Self::CsrfInvalid | Self::CredentialViaWebOnly | Self::NotInvited => {
                403
            }
            Self::NotFound => 404,
            Self::RuleConflict { .. } => 409,
            Self::ConditionInvalid(_) | Self::RegexInvalid(_) => 400,
            Self::QuotaExceeded | Self::RateLimited => 429,
            Self::Db(_) | Self::Other(_) => 500,
        }
    }

    /// 包络里的 `message` 字段 —— **对外**，所以不能等于 `to_string()`。
    ///
    /// `Db(_)` / `Other(_)` 走 `transparent`，它们的 Display 是底层错误全文：
    /// sqlx 的 message 带表名、列名、约束名，而约束名本身就是侧信道
    /// （spec/05 §3.5 实测：`uq_…` 这类名字可被用来探测他租户是否存在某条记录）。
    /// 所以这两个变体在这里返回定值，细节只进日志 —— 那边才用 Display。
    ///
    /// 其余变体的 Display 是自己写的固定文案，不含数据，可以直接用。
    /// `ConditionInvalid` / `RegexInvalid` 带的字符串是**用户自己提交的输入**
    /// 的校验结果（spec/07 §2.1 明确 `REGEX_INVALID` 要带 regex 原始编译错误），
    /// 不是库内部信息，可以外传。
    pub fn public_message(&self) -> String {
        match self {
            Self::Db(_) | Self::Other(_) => "internal error".to_string(),
            other => other.to_string(),
        }
    }

    /// 是否该把细节写进日志并附 `request_id`。
    /// 只有 500 那两个 —— 其余都是客户端能自己纠正的输入问题，记了是噪声。
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Db(_) | Self::Other(_))
    }

    /// 显式擦除类型，产出 [`AppError::Other`]。
    ///
    /// 刻意不做成 `#[from]`：见 `Other` 变体的注释。调用点应当很少，
    /// 多起来就说明有一类错误该有自己的变体了。
    pub fn internal<E>(e: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(e))
    }
}

#[cfg(test)]
// 同 profile.rs：测试里的 unwrap/panic 就是断言。
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::AppError;

    /// spec/07 §2.1 那 11 个码，外加 `INTERNAL`。
    /// 手写清单，与 `code()` 的 match 互为对照 —— match 的穷尽性保证
    /// 「每个变体都有码」，这份清单保证「码没重复、没错配状态」。
    fn all() -> Vec<AppError> {
        vec![
            AppError::Unauthenticated,
            AppError::Forbidden,
            AppError::CsrfInvalid,
            AppError::NotFound,
            AppError::RuleConflict { existing_id: 7 },
            AppError::ConditionInvalid("size_min 给了两条".into()),
            AppError::RegexInvalid("unclosed group".into()),
            AppError::QuotaExceeded,
            AppError::RateLimited,
            AppError::CredentialViaWebOnly,
            AppError::NotInvited,
            AppError::Db(sqlx::Error::RowNotFound),
        ]
    }

    #[test]
    fn eleven_public_codes_plus_internal_all_distinct() {
        let codes: Vec<&str> = all().iter().map(AppError::code).collect();
        let mut uniq = codes.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), codes.len(), "有重复的 code：{codes:?}");
        // 11 个对外码 + INTERNAL。数字变了就是 spec/07 §2.1 改了，
        // 那时该先改文档再改这里。
        assert_eq!(codes.len(), 12);
    }

    /// 本模块存在的头号理由。按直觉写会是 403。
    #[test]
    fn cross_tenant_is_404_never_403() {
        assert_eq!(AppError::NotFound.http_status(), 404);
        assert_eq!(AppError::NotFound.code(), "NOT_FOUND");
    }

    /// 纪律 3：`sqlx::Error` 绝不透给客户端。
    ///
    /// 这条不能只靠「我们记得别写 `format!("{e}")`」—— 那是纪律，不是断言。
    /// `ColumnNotFound` 在这里当漏出物的替身：它的 Display 含那个字符串，
    /// 而真实场景里同一条路径带出来的是约束名。
    #[test]
    fn db_errors_do_not_leak_identifiers_to_clients() {
        let leaky = AppError::Db(sqlx::Error::ColumnNotFound("uq_rule_conflict".into()));

        // 先确认这个替身真的会漏 —— 否则下一条断言是空的。
        assert!(
            leaky.to_string().contains("uq_rule_conflict"),
            "替身不含标识符，这条测试证明不了任何事：{leaky}"
        );

        assert!(
            !leaky.public_message().contains("uq_rule_conflict"),
            "约束名漏进了对外文案：{}",
            leaky.public_message()
        );
        assert_eq!(leaky.public_message(), "internal error");
        assert_eq!(leaky.http_status(), 500);
        assert_eq!(leaky.code(), "INTERNAL");
        assert!(leaky.is_internal());
    }

    /// 反面：输入类错误的文案该外传，不能被上一条顺手一起压掉。
    #[test]
    fn input_errors_keep_their_message() {
        let e = AppError::RegexInvalid("unclosed group at 3".into());
        assert!(e.public_message().contains("unclosed group at 3"));
        assert_eq!(e.http_status(), 400);
        assert!(!e.is_internal());
    }

    /// 同为 403 的四个码必须彼此可分 —— 前端据此决定是重新登录还是换入口。
    #[test]
    fn four_forbidden_codes_share_status_but_not_code() {
        let four = [
            AppError::Forbidden,
            AppError::CsrfInvalid,
            AppError::CredentialViaWebOnly,
            AppError::NotInvited,
        ];
        for e in &four {
            assert_eq!(e.http_status(), 403, "{} 不是 403", e.code());
        }
        let mut codes: Vec<&str> = four.iter().map(AppError::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 4);
    }

    /// `?` 能把 sqlx 错误收进来（`#[from]`），这是库层返回具体错误的前提。
    #[test]
    fn from_sqlx_error_works() {
        fn f() -> Result<(), AppError> {
            Err(sqlx::Error::RowNotFound)?;
            Ok(())
        }
        let e = f().unwrap_err();
        assert!(e.is_internal());
        assert_eq!(e.http_status(), 500);
    }

    /// `Other` 也走 500 且不漏细节。它没有 `#[from]`，所以这里
    /// 显式 `internal()` —— 那正是设计意图（见该变体的注释）。
    #[test]
    fn other_is_internal_and_opaque() {
        let e = AppError::internal(sqlx::Error::ColumnNotFound("uq_secret".into()));
        assert!(matches!(e, AppError::Other(_)));
        assert_eq!(e.public_message(), "internal error");
        assert_eq!(e.http_status(), 500);
        assert!(e.to_string().contains("uq_secret"), "日志侧该看得见全文");
    }
}
