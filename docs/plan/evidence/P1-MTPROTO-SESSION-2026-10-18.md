# P1-MTPROTO-SESSION MTProto session 序列化与加密落库

记录日期：2026-10-18

任务 id：`P1-MTPROTO-SESSION`

判据出处：`docs/design/spec/08-账号接入.md` §1.3（会话存储不落文件，落库）、
`docs/plan/编码实施前置待办.md` §8；加密与隔离复用 `spec/05` §七、ADR-0032。
测试分层见 `docs/design/eng/03-测试策略.md` §一。

## 一、范围

- `crates/tg/src/session.rs`：`StoredSession` 实现 grammers 的 `Session` trait，
  存储形状同 `MemorySession`，另带 `snapshot`/`restore`。快照为带魔数 `TGMS` 与版本号的
  二进制格式，含 home DC、各 DC 地址与 auth key、peer 缓存、updates 状态，按键排序输出。
- `crates/tg/src/client.rs`：去掉 `MemorySession`，新增 `connect_with_session` 与
  `snapshot`（先同步更新流状态再取快照）。
- `crates/app/src/mtproto_session.rs`：`save_session` / `load_session`，只拼接快照与
  `tgm_db::credentials` 的 `store_identity_secret` / `load_identity_secret`。
- 文档：`spec/08` §1.3 的代码草图改为实际接口，`编码实施前置待办.md` §8 更新状态。

不涉及迁移、RLS、GRANT、审计断言、CI 或门禁脚本。
没有开 grammers-session 的 `serde` feature：试过后 `Cargo.lock` 增加约 300 行
（serde_with、darling、schemars、jiff 等二十余个包），已回退；最终 `Cargo.lock`
只多一行（`tgm-app` 的 dev 依赖 `grammers-session`，版本已锁定）。

## 二、设计取舍

- grammers 0.10 没有 `Session::save()`，`spec/08` 原草图的接口不存在。
- 查 `PeerId::self_user()` 时按 `is_self` 找登录用户，与 `SqliteSession` 一致；
  `MemorySession` 按键直查，查不到。grammers 的 `stream_updates` 靠这次查询判断是否已登录。
- 恢复时截断、未知标签、重复键、尾随字节一律拒绝；元素计数超过剩余字节数即判无效，
  防伪造计数导致长循环。
- 快照含 auth key，等同登录凭据：只以 `Secret<Vec<u8>>` 输出，一次按估算容量分配，
  避免扩容在旧缓冲留副本；`StoredSession` 的 `Debug` 恒为 `StoredSession([REDACTED])`。
- 留一条 `ponytail:`：peer 缓存不设上限，快照随会话见过的 peer 线性增长。

## 三、验证记录

- `cargo test -p tgm-tg`：29 项通过。session 新增 7 项：完整往返（auth key、self 用户、
  频道、updates 状态，且二次快照逐字节相同）、空 session 恢复后未登录、每个截断位置与
  尾随字节被拒、未知版本与魔数被拒、超大计数被拒、`Debug` 脱敏；client 新增 1 项，
  用恢复的 session 建连（无网络）后快照仍保留 home DC。
- `cargo test -p tgm-app --test mtproto_session`：4 项以受限角色 `app_user` 连接通过。
  客户端快照落库后换新连接池恢复并建连，快照逐字节相同，库中不是明文；
  已登录 session 保留 auth key 与 self 用户；跨租户读取返回空、覆盖被拒且原数据不变；
  截断明文被拒，不部分恢复。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过。

## 四、反证

反证改动只在本地临时做，均已还原：

1. 恢复时丢掉 auth key：往返测试报红。
2. 去掉尾随字节检查：截断与尾随测试报红。
3. self 查询退化为按键直查：第一版往返测试仍绿，因为它比较的是两个 session 的
   self 查询结果，查不到时两边都是 `None`。已改成断言恢复后能查到 id 为 4242 的登录用户，
   重做反证后报红。

## 五、门禁

`scripts/ai-driver/ai-gates.sh` 完整本地门禁 rc=0：fmt、clippy、offline-build、deny、newtype、
openapi-check、driver-regressions、migrate、sqlx-check、migrate-idempotent（19 个文件两遍
逐字节相同）、test（含上述新增测试）、audit-rls、double-pass 全部通过。
门禁之后没有再改代码。

## 六、未验证

- 真机：QR 登录后落库、重启进程恢复授权、DC 迁移后的 home DC 落库（`编码实施前置待办.md` §4）。
- 落库时机（登录完成、`set_home_dc_id` 之后、每 60s 定时）的调度属于 mtproto-worker
  运行链，尚未接入。
- 真实 `KmsKek` 下的 `Decrypt` 运行期调用方：本轮新增的 `load_session` 是第一个，
  但没有在真实 KMS 下跑过。
- `api_hash_ciphertext` 列仍未使用。
