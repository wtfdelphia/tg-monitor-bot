//! 全部业务逻辑：fanout / rule / delivery / control。
//!
//! 本 crate 目前只有骨架。spec/02 §一 划的 7 个组件在这里是模块，不是 crate ——
//! 1 人全职下 7 个 crate 是净负担（eng/00 §一）。
//!
//! 一条评审纪律：本 crate 里出现 `admin_tx` 就是评审阻塞项，
//! 它意味着某段业务逻辑绕过了租户谓词（eng/00 §4.2）。
