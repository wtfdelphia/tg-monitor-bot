# rustls 漏洞门禁修复

记录日期：2026-10-15。任务 id：`DEP-rustls-2026-10-15`。

判据出处：`docs/design/eng/04-CI门禁.md` §四的 `cargo deny check`。本项独立于
`WP-5-delivery-payload-freeze`，只修改 `Cargo.lock` 中的传递依赖，不放宽门禁。

## 变更与原因

此前完整门禁在 `deny` 失败，锁定的 `rustls 0.23.45` 命中
`RUSTSEC-2026-0346`、`RUSTSEC-2026-0347`、`RUSTSEC-2026-0348`、
`RUSTSEC-2026-0349`。RustSec 对应公告均列出修复范围 `>=0.23.46`。

执行 `cargo update -p rustls --precise 0.23.46` 后，仅该包的版本和 checksum
改变。没有修改 `Cargo.toml`、`deny.toml` 或 CI，没有增加忽略项。锁文件中其他
包保持原版本。

## 验证

- 本地 `cargo-deny 0.20.2` 的 `cargo deny check` 通过，输出
  `advisories ok, bans ok, licenses ok, sources ok`。
- 另从 `EmbarkStudios/cargo-deny` 官方 release 下载 CI 固定的 `0.19.1`，
  校验归档 SHA256 后运行同一命令，也得到上述通过结果。没有替换系统工具。
- 同版工具日志：`/tmp/tgm-rustls-deny-0.19.1-2026-10-15.log`；完整门禁及其
  最终结果见 `WP-5-投递请求快照与重启-2026-10-15.md`。

这证明当前依赖树通过本次拉取的漏洞与许可检查，不保证以后不会出现新公告。
本地工具与 CI 的版本漂移仍保留，由同版补验记录说明，不修改 CI 消除警告。
