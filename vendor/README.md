# vendor/

本目录放打了本地补丁的上游 crate，经根 `Cargo.toml` 的 `[patch.crates-io]` 接入，
不是 workspace 成员。每个补丁都写清楚改了什么、为什么改，以及什么时候删。

## grammers-session 0.10.0

- 来源：crates.io `grammers-session 0.10.0` 原样拷贝，未改版本号。
  作者 Lonami Exo，`MIT OR Apache-2.0`（见 `grammers-session/Cargo.toml`）。
  crates.io 的发布包里没有 LICENSE 文件，所以本目录也没有。
- 删除的内容：`tests/deps.rs`（它 `include!` 上游仓库外的文件，在包外无法编译）、`Cargo.toml.orig`。
- 补丁位置：`src/message_box/mod.rs` 的 `process_updates` 中 `ChannelTooLong` 分支，带
  `tg-monitor-bot 本地补丁` 注释；测试在 `src/message_box/tests.rs` 末尾两条 `test_channel_too_long_*`。
- 原因：`updates.getDifference` 的 `other_updates` 里带 `updateChannelTooLong { pts }` 时，上游会把
  已知频道的本地 pts 覆盖成服务端当前 pts，再从那里取 `getChannelDifference`，
  离线期间的频道消息被静默跳过。真机实测与诊断见
  `docs/plan/proposals/WP-2-live-updates.md`「执行记录」。补丁后只在本地不认识该频道时才用服务端 pts 建条目。
- 反证：补丁前 `test_channel_too_long_keeps_known_channel_pts` 失败（取差量的起点是 505 而不是 502），
  补丁后 21 条单元测试全部通过。
- 上游状态：截至 2026-10-21，crates.io 上的 `0.11.0` 与 Codeberg master 的同一段逻辑都没变。
- 删除条件：上游发布含等价修复的版本，并且升级 grammers 全家时一起删掉本目录和 `[patch.crates-io]` 一节。
