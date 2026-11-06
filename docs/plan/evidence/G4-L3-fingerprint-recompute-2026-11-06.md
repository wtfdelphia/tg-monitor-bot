# G4 L3 指纹重算验收证据（2026-11-06）

任务：`G4-L3-notify-dedup` 的 L3 重算补验。

基线：`cloud/dev@c9570a543078dcf75e4cf90d96fd02cfdb1c2698`。验证在独立
`sqlx::test` 临时数据库中执行，不连接生产 PostgreSQL、Telegram、Redis、KMS 或对象存储。

判据出处：`docs/plan/proposals/G4-L3-notify-dedup.md` §2.1、§2.3、§3.4；
`docs/design/spec/03-数据模型.md` §5.3；`docs/design/spec/04-数据生命周期与容量.md` §二。

## 验证形状

用例：`crates/db/tests/notify_dedup.rs` 的
`sent_fingerprint_recomputes_from_retained_event_and_becomes_unavailable_after_scrub`。

1. 创建合成租户、来源、规则和规范事件，关键词保存在 `tenant_events.matched_keywords`，
   消息字段保存在 `canonical_events`。
2. 通过正式入队、领取和发送确认路径生成 `notify_dedup` 的 `SENT` 记录。
3. 沿 `notify_dedup.SENT → delivery_tasks.DONE → tenant_events → canonical_events` 找到
   最近成功任务的来源，使用生产同源 `notification_fingerprint()` 重算并与已存指纹比较。
4. 执行既定 sender 脱敏，清空 `sender_id`、`text`、`caption` 和 `content_hash`；再次读取同一
   关联链，确认源字段为空且重算结果不再等于原指纹。

## 结果

```text
DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control \
SQLX_OFFLINE=true cargo test -p tgm-db --test notify_dedup -- --nocapture

running 3 tests
test same_target_claims_once_and_success_confirms ... ok
test sent_fingerprint_recomputes_from_retained_event_and_becomes_unavailable_after_scrub ... ok
test dead_task_releases_claim_and_other_target_does_not_block ... ok

test result: ok. 3 passed; 0 failed
```

结论：隔离样本的保留事件指纹可重算；脱敏事件按 `not_recomputable` 处理，未伪造一致。
这项证据不覆盖真实 Telegram 外部投递、响应丢失、生产副本或全量历史数据扫描，也不单独
宣布 G4、G3 或 WP-8 通过。

## 门禁补验（2026-11-07）

- `scripts/ai-driver/ai-gates.sh --static-only`：通过；`fmt`、`clippy`、`offline-build`、
  `deny`、`newtype`、`openapi-check` 和 `driver-regressions` 均通过。
- `scripts/ai-driver/ai-gates.sh --db-only`：在按 `scripts/init/01-roles.sql` 初始化的
  一次性 Compose PostgreSQL 上通过；`migrate`、`migrate-idempotent`、`audit-rls` 和
  `double-pass` 均通过，审计为 28 条断言、0 条报红。
- 共享的既有验收库未被重置。该库的 `_sqlx_migrations` 记录了另一份 63 号迁移校验和，
  直接运行会按“已应用迁移被修改”拒绝；按迁移只增不改纪律，本次改用干净临时实例验证，
  不篡改历史库。
- 静态门禁首次全量编译生成的本 worktree 专属 `target/` 已在进程结束后用 `cargo clean`
  清理；一次性数据库、容器和临时数据库均已清理。

```text
[report]
task: G4-L3-fingerprint-recompute
path: ok
gates: targeted-pass
escalation: ->
```
