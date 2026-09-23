//! 敏感值的包装类型。定义处是 docs/design/eng/00-工程约定.md §五。
//!
//! 它挡的是一条具体的路径：`tracing` 的 `?value` 与 `#[instrument]` 都走
//! `Debug`，所以只要 `Debug` 恒为 `[REDACTED]`，误写 `tracing::info!(?token)`
//! 也不会把值打出去。**靠人记住「不要打印」不可靠，靠类型可靠。**
//!
//! 它挡不到的那条路径写在同一节里，一并记在这里免得误以为有保护：
//! sqlx 的 query log 打的是**绑定参数**，那是在 sqlx 内部格式化的，
//! 不经过本文件的 `Debug`。那条靠 `RUST_LOG` 里的 `sqlx=warn`（eng/01 §4.5）。

use zeroize::Zeroize;

/// 包一层就不会被 `Debug` 打出来。
///
/// 没有 `Deref`、没有 `Display`、没有 `Clone`：取值只有 [`Secret::expose`]
/// 一条路，而那个名字本身就是评审要找的关键词。
/// 与 `AdminPool`（db crate）同一套思路 —— 误用要显式写出来才能通过编译。
///
/// **`T: Zeroize` 这个 bound 是 eng/00 §五 那段代码草图上没有的**，加它的理由
/// 是编译器逼出来的：草图里 `Secret<T>` 无约束，而 `Drop` 里要清零就得有
/// `T: Zeroize`，Rust 不允许 `Drop` impl 比类型本身要求更严（E0367）。
/// 两条出路 —— 要么放弃自动清零、要么把 bound 提到类型上。取后者，因为
/// ADR-0012 把「返回值用后必须 zeroize」写进了 `Kek` 的签名要求，
/// 靠调用方记得清零正是这个类型要消灭的那种纪律。
/// 代价：包一个不实现 `Zeroize` 的类型时要先给它实现（`zeroize` 有 derive）。
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }

    /// 取出内层值。**调用点是评审面**：搜 `expose` 能枚举出全部取值位置。
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// `Drop` 时清零。
///
/// **清零不等于「这个值在内存里没有别的副本」**：`Secret<String>` 在扩容时
/// 会留下旧缓冲区的内容，这里只清当前那一块；把值 move 进 `Secret` 之前的
/// 那份拷贝也不在管辖范围内（`kek.rs` 里两处手工 `zeroize` 就是补这个）。
/// 它保证的是「这一份不会留在释放后的内存里」，仅此而已。
impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        // 顺带确认它不是「Debug 里恰好没出现这个词」而是真的固定输出：
        // 换一个值，输出必须一字不变。
        let other = Secret::new("完全不同的值".to_string());
        assert_eq!(format!("{other:?}"), format!("{s:?}"));
    }

    #[test]
    fn debug_of_nested_container_is_redacted() {
        // 这条比上一条有用：实际泄漏形态通常是 Secret 被塞进某个结构体，
        // 而那个结构体 #[derive(Debug)]。派生的 Debug 会调用本文件的实现。
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Cred {
            id: i64,
            token: Secret<String>,
        }
        let c = Cred {
            id: 7,
            token: Secret::new("hunter2".to_string()),
        };
        let out = format!("{c:?}");
        assert!(
            out.contains("[REDACTED]"),
            "派生 Debug 没走到脱敏实现：{out}"
        );
        assert!(!out.contains("hunter2"), "token 泄漏进 Debug：{out}");
        assert!(
            out.contains('7'),
            "非敏感字段也被吞了，那说明包错了层：{out}"
        );
    }
}
