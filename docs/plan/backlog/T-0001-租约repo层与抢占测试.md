# T-0001：租约 repo 层与抢占测试

status: done
wp: WP-2
escalation: no
criteria: plan/00 §4.3 WP-2 出口判据「租约抢占行为可复现」；判据形状照 eng/03 §三 R11 的幂等风格

`account_leases` 表已存在（迁移见 `migrations/`，丙类表、不启 RLS，理由在
`docs/design/spec/03-数据模型.md` §1.3）。本任务做它的 repo 层：

```text
1 acquire(identity_id, worker_id) → Option<fencing_token>
  首次获取从 1 开始；同 worker 重复获取只刷新租约、不改变 token；
  过期接管递增 token；其他 worker 持有有效租约时返回空行
2 renew(identity_id, worker_id, fencing_token) → bool
  只接受当前 worker 的当前 token，且过期租约不得复活；成功续租不改 token
3 release(identity_id, worker_id, fencing_token) → bool
  只让当前持有者立即过期租约，保留行和 token，避免释放后 token 回退
```

实现位置与错误纪律照 `docs/design/eng/00-工程约定.md` §三（thiserror 库层）。

验收判据（L2 测试，照 `docs/design/eng/03-测试策略.md` §二 的三角色纪律：
repo 层 CRUD 断言用 `#[sqlx::test]` 注入池即可；**凡验证角色权限或隔离
形状的断言，必须换到受限角色池**，不得用注入池 —— 它在临时库上带
`tgm_test_admin` 恒真策略，隔离断言用它跑是假绿）：

```text
a 首次 acquire → token = 1
b 未过期且他人持有 → 返回空行（不得等待、不得报错）
c 把 expires_at 拨到过去再抢占 → 成功，token = 2
d 并发：两个连接同时抢占已过期的租约 → 恰好一个成功
  （并发形状照 eng/03 §3.6 R18 的教训：照字面写会退化成顺序执行）
e 续租后 heartbeat_at/expires_at 前进，token 不变
f 旧 worker 或旧 token 续租失败；释放后重新获取 token 继续递增
```

边界：不碰 MTProto 连接、不碰 `account_leases` 的表结构（改表命中升级条件）。
全部卡口绿（`scripts/ai-driver/ai-gates.sh`）。已完成的 repo 与测试位于
`crates/db/src/repo.rs`、`crates/db/tests/lease.rs`；真机混沌清单仍属于 WP-2
后续出口，不在本任务伪装成已通过。
