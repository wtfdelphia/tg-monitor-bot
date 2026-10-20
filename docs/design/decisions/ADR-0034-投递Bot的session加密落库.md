# ADR-0034：投递 Bot 的 MTProto session 加密落库，重启复用

状态：已定案（2026-10-20）

## 决定

- delivery 登录成功后，把 Bot 的 MTProto session 加密写进 `identity_secrets` 同一行的
  `bot_session_ciphertext` / `bot_session_nonce`。下次启动先用这份 session 建连，
  以 `users.getUsers(self)` 确认仍有效，不再调 `auth.importBotAuthorization`。
- session 与 Token 用同一个 DEK 加密，AAD 前缀不同。不另起 DEK：两份密文同生同灭，
  分开包裹会让「Token 换了、session 还在」成为可能。
- Token 被覆盖（control-plane 重写凭据）时，同一条写入语句清空 session。delivery 写回 session
  时校验 DEK 密文仍是读出时那一份，不是就丢弃。
- session 失效（auth key 被注销、快照解不开或解析失败、登录的不是这个 Bot）时清掉 session，
  退回 Token 登录；Token 本身被拒仍按 ADR-0033 转 `unauthorized`。
- 取代 [[ADR-0033]] 决定节最后一条「Bot 的 MTProto session 只在进程内存里，不落库」。
  ADR-0033 的其余条款不变。

## 背景

ADR-0033 不落 session 的理由是「Bot 凭 Token 随时可重新登录，持久化换不来恢复能力」，
代价节写明「重启风暴下的限流没有测过」。常驻长跑把它测出来了：同一个 Bot 几分钟内
第 4 次左右 `bot_sign_in` 开始被 `FLOOD_WAIT`，等待 66–305 秒，期间该 Bot 的投递停住。
崩溃循环、频繁发布、运维反复重启都会撞上。

三个选项：

- **A 不落库**：零 schema 改动，接受重启频繁时投递停几分钟。
- **B 加密落库**：重启复用 auth key，不再消耗登录配额；多一份凭据密文与一份迁移。
- **C 只加告警**：停顿还在，只是能看见。

选 B。A 与 C 都把一个可预见、可复现的停顿留给生产；B 的代价在凭据层已有的信封加密里
就能收住，不新增密钥、不新增 IAM 权限。

## 代价

- 库里多一份 Bot 登录态。auth key 与 Token 泄露面相当：拿到它的人在 Bot 被重置前都能以
  该 Bot 行事。它与 Token 同行、同 DEK、同样只有持 Decrypt 的角色解得开，没有扩大面，
  但「重置 Token 即失效」对 session 不成立 —— BotFather 重置 Token 不一定注销已有 auth key，
  这一点没有真机验证。平台侧的兜底是 control-plane 重写 Token 时清空 session。
- delivery 写回 session 需要 unwrap 一次 DEK（session 必须用 Token 那份 DEK 加密）。
  delivery 本来持有 `kms:Decrypt`，权限面不变，但每次 Token 登录多一次 KMS 往返。
- `identity_secrets` 原本「一行一份凭据」，现在 Bot 行有两份密文。MTProto 行的两列恒为空，
  没有用 CHECK 约束 `kind`：`identity_secrets` 不带 `kind` 列，约束要跨表，不值得。
- session 损坏时静默退回 Token 登录，只留一条日志，不写审计。篡改 session 列的人
  最多让 Bot 多登录一次。
- 单实例前提不变（[[ADR-0025]]）：两个 delivery 进程同时写回 session 时后写者胜，
  两份都有效，没有正确性问题。

## 相关

[[ADR-0025]]、[[ADR-0032]]、[[ADR-0033]]。
提案 `docs/plan/proposals/WP-5-delivery-bot-session.md`；
限流实测 `docs/plan/evidence/WP-5-delivery常驻长跑-2026-10-19.md`；
落地在 `spec/03-数据模型.md`（`identity_secrets`）、`ops/00-部署与运行时.md`（delivery 角色）。
