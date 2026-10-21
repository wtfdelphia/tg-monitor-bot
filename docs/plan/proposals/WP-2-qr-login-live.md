# WP-2-qr-login-live：QR 登录全分支真机实测

状态：片 1 已批准并完成（2026-10-21），T1–T6 全部 pass，T6 经最小修复后通过。证据见 `docs/plan/evidence/WP-2-qr-login-live-2026-10-21.md`。片 2 未开始。

## 任务与判据

任务 id：`WP-2-qr-login-live`。

判据出处：
- `docs/design/plan/00-里程碑与工期.md` §4.3 WP-2 出口判据「QR 全分支实测（含 2FA 与 DC 迁移）」；
  §三 G1 第二项「QR 登录全分支（含 2FA、`MigrateTo`）」，以及 G1 关于 RK-01 的处置。
- `docs/design/spec/08-账号接入.md` §3.4 状态机、§3.5 动工前必须实测的六项、§五 身份状态机。
- `docs/plan/evidence/P0-外部验证手册-2026-10-08.md` §2.2 的 T1–T6 判据与脱敏要求。
- `docs/design/plan/01-风险登记.md` RK-01。

按 `AGENTS.md` §五第 5 条（Telegram 真机：QR 扫码、2FA、DC 迁移、真凭据）升级，不直接实现。
`docs/plan/backlog/T-0002-扫码登录状态机-升级任务样例.md` 仍为 `escalated`，本提案承接它的
「必须升级的实测项」。

## 现状（2026-10-21 核对代码与证据）

- `crates/tg/src/qr.rs`：纯状态机与 L1，不连网。
- `crates/tg/tests/qr_login.rs`：`#[ignore]` 测试工具 `live_user_qr_login`，走
  `exportLoginToken` → 轮询 → `MigrateTo` 时切 home DC 再 `importLoginToken` → 可选 2FA，
  session 写仓库外文件。它服务于 WP-5 媒体中转的真机验收，**没有按 T1–T6 逐项留证**；
  WP-5 的证据文件均写明「不宣布 WP-2 的 QR 出口判据通过」。
- 已有的真实观察：测试用户账号扫码成功，home DC 5（grammers 起手连 DC 2）。工具当时的输出
  `migrations=` 与 `2fa=` 没有写入任何证据，所以「是否真的走了 MigrateTo」「是否真的走了 2FA」
  目前没有书面记录。RK-01 推测的「素材可能已现成」很可能成立，但未确认。
- 生产路径没有 QR 登录：控制面没有 `/api/v1/identities/login/qr*` 端点，`login_sessions` 没有任何
  代码读写，MTProto 身份没有创建入口；spec/08 §1.3 写明「登录完成后的那一次落库属于 QR 登录调用链，
  尚未接入」。
- session 加密落库与重启恢复（T5 的生产侧）已有：`crates/app/src/mtproto_session.rs` 的
  `save_session` / `load_session`，mtproto-worker 每 60s 与退出前落库，L2 覆盖。
- 授权失效（T6 的生产侧）已有：`is_unauthorized` 识别 `AUTH_KEY_UNREGISTERED`、`SESSION_REVOKED` 等，
  worker 转 `unauthorized`、释放租约、写审计，L2 覆盖；**用户账号**在手机端撤销会话的真机路径没测过
  （Bot 的 Token 作废已真测，见 `WP-5-delivery真机验收-2026-10-19.md` V3）。

## 建议切片

### 片 1：六项真机实测（本提案请求批准的范围）

不新增迁移、不改 RLS/GRANT/审计、不加依赖、不进生产代码。只改测试工具与证据：
（若真机暴露生产代码缺陷，例如 T6 识别不到撤销，最小修复在批准范围内，补 L1/L2 并记证据。）

1. 扩展 `crates/tg/tests/qr_login.rs` 的 `live_user_qr_login`，按 T1–T4 输出逐项可判定的行：
   每代 token 的 generation 与过期时间（不输出 token）、是否收到 `MigrateTo`（起止 DC）、
   是否进入 `SESSION_PASSWORD_NEEDED`、错误密码是否不推进、最终 home DC。
   状态推进同时喂给 `tgm_tg::qr::QrLoginState`，让真机事件序列也走一遍纯状态机，
   序列不合法即失败。
2. T4 新增 ignored 用例：导出 token 后不扫码等它过期，再用旧 token `importLoginToken`，
   预期被拒（记录错误码）；随后新 token 可继续登录。
3. T5 新增 ignored 用例（放 `crates/app/tests/`，`tgm-tg` 不依赖数据库）：用扫码得到的 session 走生产路径 —— `save_session` 加密落库
   （sqlx::test 临时库、`app_user` 连接、开发 EnvKek）→ 丢弃客户端 → `load_session` 恢复 →
   `connect_with_session` 后 `getMe` 仍是同一用户，不要求再扫码。
4. T6 新增 ignored 用例（同放 `crates/app/tests/`）：用户在手机端「设备」里结束本会话后，用该 session 跑一次
   `run_identity`（mtproto-worker 的单身份循环），预期退出为 `Unauthorized`、
   `identities.status = unauthorized`、`identity.unauthorized` 审计一条、租约可被接管。
5. 证据：`docs/plan/evidence/WP-2-qr-login-live-<日期>.md`，逐项填 pass / fail / 未验证，
   只保留时间、DC、generation、错误类别；不保留手机号、`api_hash`、session、二维码内容、密码。

T2（MigrateTo）的素材按两条路径尝试，任一成立即可：

- 现有测试账号 home DC 5，而 grammers 起手连 DC 2：重新扫码时应当收到 `MigrateTo 2 → 5`。
  工具会把这一事实打印出来；成立则 T2 用真实生产 DC 完成。
- 不成立时，按外部验证手册用 Test DC 保留号码。这需要额外改造工具连 Test DC，
  另记一条，不在片 1 默认范围。

两条都不成立时，T2 登记 RK-01 未验证，按 plan/00 G1 处置「不阻塞 G1，G4 前补验」，不伪造通过。

需要你配合的事：

```text
1 用测试用户账号重新扫码（T1/T2，可能还有 T3）；扫码前我先移走现有 user.session。
2 T3：测试账号临时开启 2FA，测试结束后可关闭；密码沿用 2fa.txt 文件交付方式（读完即删）。
  T3 还要求「错误密码不推进」：先给一次错误密码，再给正确密码。
3 T4：二维码出现后不扫，等它过期（约 30 秒），我验证旧 token 被拒，再扫新码。
4 T6：T5 通过后，在手机端「设置 → 设备」结束这台测试会话，再通知我跑 T6。
  T6 会让当前 user.session 永久失效，WP-5 的媒体真机工具之后要重新扫码。
```

### 片 2：QR 登录接入控制面（不在本次批准范围）

实现 spec/07 §3.3 的 `/api/v1/identities/login/qr*` 与 `login/password`，使用 `login_sessions`、
新建 MTProto 身份并首登落库、`tenant_consents` 代持授权（spec/06 §2.2）、QR 轮询限流。
它触碰凭据录入、可能需要迁移（`login_sessions` 现无 `identity_id` / 代次 / 密文列，
login_token 是 bearer 凭据却明文 BYTEA）与安全面，片 1 有结论后另写提案。

## 结论口径

片 1 全部通过只说明「QR 登录在真机上可行、各分支行为符合 spec/08 §3.4」，满足 G1 第二项的可行性判定
与 WP-2 出口判据的「实测」部分；**不等于** QR 登录已作为产品功能交付（那是片 2）。
WP-2 出口判据的另一半「租约抢占行为可复现」已由 T-0001 与 `WP-2-mtproto-worker运行链-2026-10-19.md`
覆盖，本提案不重复。

## 需要你决定的事

1. 是否批准片 1 的范围（改 `crates/tg/tests/qr_login.rs`，在 `crates/app/tests/` 新增 ignored 真机用例，新增证据）。
2. T3 是否允许测试账号临时开启 2FA。
3. T6 是否允许在 T5 之后撤销当前测试会话（之后 WP-5 真机工具需重新扫码）。
