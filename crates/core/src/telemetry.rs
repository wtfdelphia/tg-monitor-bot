//! subscriber 初始化。形态定在 eng/00 §5.1，脱敏约定在 §五。
//!
//! **为什么形态在首发就定死**：`registry()` + `layer` 是后面接 OTel 的前提，
//! 接链路时只多挂一层 `tracing_opentelemetry::layer()`。写成 `fmt::init()`
//! （单 subscriber、无 registry）的话，接链路要把初始化整个重写。
//!
//! 本模块与 `config.rs` 分管 `sqlx=warn` 的两半，合起来才是 §五 第 3 条：
//!   - `config.rs`  `RUST_LOG` **显式给了**却不含 `sqlx=warn` → production 下拒绝启动
//!   - 本模块       `RUST_LOG` **没给**时的默认值必须含 `sqlx=warn`
//!
//! 两半都要在，因为漏掉任一半的后果相同而征兆不同：sqlx 把绑定参数打进日志，
//! 而 `identity_secrets` 的写入语句绑的就是密文与 nonce。`Secret<T>` 管不到
//! 这条路径 —— 参数是在 sqlx 内部格式化的，不经过我们的 `Debug`（§五 末段）。

use tracing_subscriber::EnvFilter;

/// `RUST_LOG` 缺省时的过滤器。**`sqlx=warn` 是安全配置，不是调噪声**。
///
/// 做成 `pub const` 而不是内联在 `init()` 里，是为了让下面那条测试能断言它 ——
/// 一个内联的字符串字面量只能靠读代码确认，而这一条值得有断言。
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// 装 subscriber。**全进程只许调一次**，重复调用会 panic（`try_init` 那版
/// 返回 Err，但静默忽略等于让第二次配置无声失效）。
///
/// `json` 与 `pretty` 的选择按 profile 走，依据是 eng/00 §5.1 那两行：
/// 生产 JSON 单行进 stdout 由容器运行时收走；本地人读格式。
/// 不自己写文件、不自己轮转（不引 `tracing-appender`）—— 日志归运行时管。
pub fn init(profile: crate::profile::Profile) {
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env()
        // 这里**不能**写成 `EnvFilter::new("info")` —— 见 DEFAULT_FILTER 的注释。
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let registry = tracing_subscriber::registry().with(filter);

    // 两支只能各自 .init()，不能把 layer 存进变量再统一 init：
    // fmt::layer().json() 与 .pretty() 的具体类型不同，装不进同一个绑定。
    // Box<dyn Layer> 能统一，但那是为了省三行而引入一层动态派发。
    if profile.is_production() {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true),
            )
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().pretty())
            .init();
    }
}

#[cfg(test)]
// 同 profile.rs：测试里的 unwrap/panic 就是断言。
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::DEFAULT_FILTER;
    use tracing_subscriber::EnvFilter;

    /// §五 第 3 条的默认值那一半。这条测试就是这个常量存在的理由。
    #[test]
    fn default_filter_silences_sqlx() {
        assert!(
            DEFAULT_FILTER.contains("sqlx=warn"),
            "默认过滤器不含 sqlx=warn —— sqlx 会把绑定参数打进日志，\
             而 identity_secrets 的写入绑的是密文与 nonce（eng/00 §五 第 3 条）"
        );
    }

    /// 上一条只查了字符串里有没有那几个字符，**那不等于过滤器真的生效**：
    /// 拼错成 `"info,sqlx-warn"` 同样含 `sqlx`，而 `EnvFilter` 对无法解析的
    /// 指令是**跳过**而非报错 —— 于是拼错的形式与正确的形式在字符串层面同形。
    ///
    /// 所以这里装一个真的 subscriber，直接问它「sqlx 的 INFO 事件会不会发出」。
    /// 中间试过断言 `EnvFilter::to_string()`，那是它的 Display 形态而不是行为，
    /// 换个版本就可能变；`event_enabled!` 问的是实际决策。
    ///
    /// 用 `with_default` 而不是 `init()`：`init()` 是进程全局且只许一次，
    /// 同一测试二进制里的多条测试会互相抢。
    #[test]
    fn default_filter_actually_blocks_sqlx_info_events() {
        use tracing_subscriber::prelude::*;

        let sub = tracing_subscriber::registry().with(
            DEFAULT_FILTER
                .parse::<EnvFilter>()
                .expect("默认过滤器解析失败"),
        );

        tracing::subscriber::with_default(sub, || {
            assert!(
                !tracing::event_enabled!(target: "sqlx::query", tracing::Level::INFO),
                "sqlx 的 INFO 事件能发出 —— 绑定参数会进日志（eng/00 §五 第 3 条）"
            );
            // 反证这个 subscriber 不是把一切都拦了 —— 否则上一句在任何
            // 过滤器下都成立，包括一个彻底写坏的。
            assert!(
                tracing::event_enabled!(target: "tgm_core::probe", tracing::Level::INFO),
                "默认过滤器把业务 INFO 也拦了 —— 上一句断言随之失去意义"
            );
        });
    }

    /// 反面：一个**没有** sqlx 指令的过滤器必须让上一条的第一句断言失败。
    /// 没有这一条，「event_enabled 对 sqlx 返回 false」可能只是因为
    /// 探针的 target 写错了 —— 那样它对任何过滤器都返回 false。
    #[test]
    fn counterproof_a_filter_without_sqlx_lets_it_through() {
        use tracing_subscriber::prelude::*;

        let sub = tracing_subscriber::registry().with("info".parse::<EnvFilter>().unwrap());
        tracing::subscriber::with_default(sub, || {
            assert!(
                tracing::event_enabled!(target: "sqlx::query", tracing::Level::INFO),
                "连 `info` 都不让 sqlx 的 INFO 通过 —— 说明上一条测试的探针\
                 恒为 false，它什么都没验"
            );
        });
    }
}
