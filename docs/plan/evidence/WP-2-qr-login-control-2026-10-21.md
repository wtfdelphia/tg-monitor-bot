# WP-2 QR 登录接入控制面

记录日期：2026-10-21

任务 id：`WP-2-qr-login-control`（提案 `docs/plan/proposals/WP-2-qr-login-control.md`，片 2）

判据出处：`docs/design/spec/07-接口契约.md` §3.3；`docs/design/spec/08-账号接入.md` §1.3、§3.4、§五；
`docs/design/spec/06-合规与凭据责任.md` §2.2；`docs/design/spec/02-总体架构.md` §6.3；
`docs/design/plan/00-里程碑与工期.md` §4.1「同意文本与落库进 WP-2」。

决定口径：D1–D4 按提案推荐方案实现，D5 只交付 API。这五项和新增依赖 `qrcode` 还需要人确认。

## 一、交付

| 层 | 位置 | 内容 |
|---|---|---|
| tg | `crates/tg/src/qr_login.rs` | `QrLoginDriver`：从片 1 测试工具提出的生产驱动（export → 轮询 → `MigrateTo` → 2FA → session 快照），状态推进走 `QrLoginState`；已授权但不落库时 `log_out`（`auth.logOut`） |
| db | `crates/db/src/credentials.rs` | `create_mtproto_identity`、`reauthorize_mtproto_identity`、`load_mtproto_identity`、`latest_custody_consent`、`insert_custody_consent`；`api_id` 落库，`api_hash_ciphertext` 置 NULL |
| app | `crates/app/src/qr_login_control.rs` | 进程内登记簿与限流（带 `ponytail:` 注释） |
| app | `crates/app/src/control.rs` | `POST /api/v1/identities/login/qr`、`GET /api/v1/identities/login/qr/{login_id}`、`POST /api/v1/identities/login/password` |
| 契约 | `openapi.json` | 三个端点与四个 schema；`tgm openapi --check` 一致 |
| 文档 | spec/07 §3.3、spec/08 §1.3 与 §3.4、spec/03 `login_sessions` | 改写为实现现状 |

不新增迁移，不改 RLS / GRANT / 审计断言 / CI。新增依赖：`qrcode =0.14.1`（`default-features = false, features = ["svg"]`），
`Cargo.lock` 只多这一个包；`cargo deny check licenses` 通过。

## 二、测试

L1（`crates/app/src/qr_login_control.rs`，6 条）：`login_id` 是 32 字节随机数；跨租户、跨用户、不存在
同口径 `NotFound`；轮询与提交密码各自 2 秒间隔；每租户并发 2 个；每小时 10 次滑动窗口；超时条目移出且释放并发名额。

L2（受限角色 `app_user`）：

- `crates/db/tests/credentials.rs`
  - `create_mtproto_identity_is_encrypt_only_audited_and_deduplicated`：只有 wrap 的 KEK 也能建身份，DEK 绑定新身份 id；
    `api_id` 落库、`api_hash_ciphertext` 为 NULL；worker 侧 KEK 能解回 session；审计不含 session；同租户重复拒绝，
    别的租户不受影响，且按 id 查不到本租户身份。
  - `reauthorize_mtproto_identity_requires_same_account_and_expires_lease`：换账号拒绝且 session 不变、不留审计；
    同账号覆盖 session 与 `api_id`，`unauthorized → offline`，在册租约立即过期而 `fencing_token` 不变；
    `banned` 保持 `banned`；跨租户 `IdentityNotFound`。
  - `custody_consent_latest_version_is_tenant_scoped`：取最近一条版本，租户隔离，`ip` 按 `inet` 落库。
- `crates/app/tests/control.rs`
  - `qr_login_start_rejects_before_any_telegram_call`：free 档 403；Bot 入口 403 `CREDENTIAL_VIA_WEB_ONLY`；
    member 403；同意版本不对 403；`api_id`/`api_hash` 格式错 400；重新授权指向别的租户的身份或 Bot 身份 404；
    未配置 KEK 500。所有拒绝都发生在连 Telegram 之前（Bot API 地址指向不可达端口），不留同意记录，
    响应带 `no-store`，不回显 `api_hash`。
  - `qr_login_poll_and_password_reject_bot_source_and_unknown_login`：轮询与提交密码经 Bot 入口 403，
    未知 `login_id` 404，响应带 `no-store`，不回显密码。

覆盖缺口：pending → confirmed 的成功路径要连 Telegram，没有 L2；由下面的真机验收覆盖。

## 三、真机验收

2026-10-21 18:03–18:08，测试用户账号（人工持有，开了 2FA），平台 `api_id` 代替租户提交的 `api_id`/`api_hash`。
工具 `crates/app/tests/qr_login_control_live.rs`（`#[ignore]`）：请求直接打 `router()`，与生产同一套 handler、
登记簿、限流与落库代码；租户上下文以 owner + Web 注入。二维码按每代覆盖写到仓库外的本地文件（0600），结束即删。

```bash
cargo test -p tgm-app --test qr_login_control_live -- --ignored --exact \
  live_qr_login_via_control_plane_then_worker_online --nocapture
```

工具输出原文：

```text
[QRC] started login_id_bytes=32 html=written
[QRC] rate_limit=429 other_user=404
[QRC] pending generation=2 … generation=8
[QRC] awaiting_password，等待 TGM_LIVE_2FA_PASSWORD_FILE
[QRC] confirmed generations=8 2fa=true password_rejected=0 ciphertext_bytes=770 api_id_set=true api_hash_stored=false kek_id=local:env:v1 consent=1 audit=1 worker_online=true exit=Shutdown
test result: ok. 1 passed; 0 failed
```

| 项 | 结果 |
|---|---|
| 发起 | 201，`login_id` 32 字节，响应不含 `api_hash`，所有响应 `Cache-Control: no-store` |
| 限流与归属 | 2 秒内第二次轮询 429；同租户另一用户轮询 404 |
| token 轮换 | 等扫码期间轮询拿到 8 代二维码，换代后 `qr_svg` 内容随之变化 |
| 2FA | 扫码后 `awaiting_password`，经 `POST /login/password` 一次通过 |
| 落库 | `confirmed`，身份 `mtproto` / `offline`；session 密文 770 字节；`api_id` 落库、`api_hash_ciphertext` 为 NULL |
| 同意与审计 | `credential_custody` / `custody-v0` 一条（带 user_agent）；`identity.create` 审计一条，`source = web`，不含 `api_hash` |
| 终结后 | 同一 `login_id` 再轮询 404 |
| worker 上线 | `run_identity` 只凭库里的 session 置 `online`，停止信号后 `Shutdown` 退出 |

未覆盖：密码错误分支（本次一次输对；片 1 的 T3 已在驱动层实测）、重新授权（`identity_id`）的真机路径、
`DC 迁移`本次是否发生没有在端点输出里打印（驱动层 T2 已实测）。

## 四、门禁

`scripts/ai-driver/ai-gates.sh` 完整运行（2026-10-21，EXIT=0）：fmt、clippy、offline-build、deny、newtype、
openapi-check、driver-regressions、migrate、sqlx-check、migrate-idempotent（22 个文件重跑，库状态逐字节相同）、
test（351 passed / 0 failed / 17 ignored）、audit-rls（18 条断言 0 条报红）、double-pass（每条断言被对应破坏单独打红）全部通过。
本地 cargo-deny 0.20.2，ci.yml 钉 0.19.1，版本漂移由脚本提示，裁决以 CI 为准。
