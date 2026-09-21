//! grammers + teloxide 封装。
//!
//! 本 crate 单独存在的理由不是整洁，是依赖退出路径：grammers 是 0.x 且
//! bus factor 低（spec/08 §1.1），把 raw invoke 收敛在一个 crate 里，
//! 将来 fork 或替换时改动面可控（eng/00 §一）。
//!
//! 所以有一条纪律：**raw invoke 不得出现在本 crate 之外。**
