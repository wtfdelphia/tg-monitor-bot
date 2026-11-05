# G4 L3 通知指纹去重实施证据（2026-11-05）

任务：`G4-L3-notify-dedup`。

基线：`cloud/dev` 合并提交 `42a2fd6`（PR #92、#93 合入后的 `dev`）。实现分支为
`codex/G4-L3-notify-dedup-impl`，工作树位于独立临时目录；主工作树未参与本片实现。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-3、§三 G3/G4；
`docs/design/eng/03-测试策略.md` L1/L2/L3；`docs/design/eng/04-CI门禁.md` 的
审计、迁移重入与双遍要求；提案 `docs/plan/proposals/G4-L3-notify-dedup.md`。

## 一、实现范围

- 在 `crates/core/src/notification.rs` 固化 `tgm:l3:v1` 指纹规范：sender、排序去重后的
  关键词、text/caption 回退和前 300 个 Unicode 标量值，输出小写 SHA-256 十六进制值。
- 在规则命中到投递入队的同一事务中创建 `notify_dedup` claim，并把
  `notification_fingerprint`、`notification_target_ref` 固化到 `delivery_tasks`。
- 发送成功将 claim 转为 `SENT`；永久失败、取消、非活跃租户 defer 和人工重试遵循提案中的
  claim 释放/重新占用语义，不绕过 L3 去重。
- 新增 `migrations/0063_g4_notify_dedup.sql` 及逐字节一致的测试迁移镜像；新增状态约束、
  任务外键、索引、清理角色可见列和过期清理策略。
- 同步 `C6` 投递不可变列审计、清理角色审计清单和相关结构体测试夹具。

## 二、验证结果

| 验证 | 结果 | 记录 |
|---|---|---|
| 指纹 L1 | `pass` | `cargo test -p tgm-core notification --lib`：3 passed / 0 failed |
| L3 claim 生命周期 | `pass` | `cargo test -p tgm-db --test notify_dedup -- --nocapture`：2 passed / 0 failed |
| 相关回归 | `pass` | `tgm-app` pipeline：6 passed；rule version retention：6 passed；unmatched retention：11 passed |
| 迁移重入 | `pass` | `scripts/migrate-idempotent.sh`：57 个迁移文件强制重跑，库状态逐字节一致；最终使用隔离克隆的非生产审计库 |
| 静态门禁 | `pass` | `scripts/ai-driver/ai-gates.sh --static-only`：fmt、clippy、offline-build、deny、newtype、openapi-check、driver-regressions 全部通过；数据库卡口按模式未运行 |
| 可信数据库门禁 | `pass` | `scripts/ai-driver/ai-gates.sh --db-only`：migrate、migrate-idempotent、audit-rls、double-pass 全部通过；28 条断言 0 条报红 |
| 空白检查 | `pass` | `git diff --check`、`cargo fmt --all -- --check` 通过 |

L3 测试覆盖：同一租户/同一目标的 claim 抑制与发送确认、死信释放后人工重试重新 claim、
不同目标不互相抑制。指纹测试覆盖关键词顺序与重复项、text/caption 回退、Unicode 标量截取、
sender 输入和版本前缀。

## 三、边界与未运行项

- 未使用真实 Telegram token、`api_hash`、session 或真实频道；限流、在途请求、响应丢失后的
  外部平台映射和目标消息核对仍为 `not-run`。
- 未连接生产 PostgreSQL、KMS、Redis、对象存储或生产调度；未执行真实租户删除。
- 本片不宣布 WP-3、WP-4、G3、G4、WP-8 或生产上线通过；PR CI 仍是提交后的最终裁决。
- 本次静态全量编译生成的专属 `target/` 已在门禁结束且无进程使用后用 `cargo clean` 清理，
  未删除源码、`.sqlx`、证据或其他 worktree 产物。

## 四、门禁收口

静态门禁、可信数据库门禁和本片定向测试均已通过。数据库验证使用从本地非生产 Compose
`tgm` 克隆的隔离数据库，并撤销 `tgm_test` 的 CONNECT 以复现生产审计边界；没有连接生产
服务。第一次使用裸 `CREATE DATABASE` 的验证因模板触发器和角色 ACL 不符合仓库边界而失败，
未计为实现失败；随后按 CI 的数据库分工重建验证并通过。

按仓库纪律，本片整理为一个提交并推送到 `dev`，再由 PR CI 裁决。

```text
[report]
task: G4-L3-notify-dedup
path: ok
gates: targeted-pass
escalation: ->
```
