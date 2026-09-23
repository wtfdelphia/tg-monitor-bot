# T-0001：租约 repo 层与抢占测试

status: pending
wp: WP-2
escalation: no
criteria: plan/00 §4.3 WP-2 出口判据「租约抢占行为可复现」；判据形状照 eng/03 §三 R11 的幂等风格

`account_leases` 表已存在（迁移见 `migrations/`，丙类表、不启 RLS，理由在
`docs/design/spec/03-数据模型.md` §1.3）。本任务做它的 repo 层：

```text
1 acquire(identity_id, worker_id) → Option<fencing_token>
  SQL 逐字照 docs/design/spec/08-账号接入.md §4.2 的 ON CONFLICT 形状：
  仅当已过期或自己续租才抢占，每次抢占递增 fencing_token，RETURNING 拿 token
2 renew = 同一条（worker_id 相同走续租分支）
3 释放（可选）：删除本 worker 持有的行
```

实现位置与错误纪律照 `docs/design/eng/00-工程约定.md` §三（thiserror 库层）。

验收判据（L2 测试，照 `docs/design/eng/03-测试策略.md` §二 的三角色纪律：
repo 层 CRUD 断言用 `#[sqlx::test]` 注入池即可；**凡验证角色权限或隔离
形状的断言，必须换到受限角色池**，不得用注入池 —— 它是超级用户）：

```text
a 首次 acquire → token = 1
b 未过期且他人持有 → 返回空行（不得等待、不得报错）
c 把 expires_at 拨到过去再抢占 → 成功，token = 2
d 并发：两个连接同时抢占已过期的租约 → 恰好一个成功
  （并发形状照 eng/03 §3.6 R18 的教训：照字面写会退化成顺序执行）
e 续租后 heartbeat_at/expires_at 前进，token 不变
```

边界：不碰 MTProto 连接、不碰 `account_leases` 的表结构（改表命中升级条件）。
全部卡口绿（`scripts/ai-driver/ai-gates.sh`）。
