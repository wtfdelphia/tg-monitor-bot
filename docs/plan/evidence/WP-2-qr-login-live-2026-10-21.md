# WP-2 QR 登录全分支真机实测

记录日期：2026-10-21

任务 id：`WP-2-qr-login-live`（提案 `docs/plan/proposals/WP-2-qr-login-live.md` 片 1，已批准）

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-2 出口判据「QR 全分支实测（含 2FA 与 DC 迁移）」、
§三 G1 第二项；`docs/design/spec/08-账号接入.md` §3.4、§3.5、§五；
`docs/plan/evidence/P0-外部验证手册-2026-10-08.md` §2.2 的 T1–T6；`docs/design/plan/01-风险登记.md` RK-01。

脱敏：本文只记时间、DC、generation、错误码与计数。不记手机号、`api_hash`、session、二维码内容、密码。
测试账号为人工持有的测试用户账号，平台 `api_id`。

## 一、结论

| 项 | 结果 | 判定依据（工具输出原文） |
|---|---|---|
| T1 扫码授权 | pass | `[QR] authorized generations=7 migrations=[(2, 5)] 2fa=true password_rejected=1 start_dc=2 home_dc=5 session_bytes=754` |
| T2 `MigrateTo` | pass | `[QR] LoginTokenMigrateTo dc 2 -> 5`：生产 DC，起手 DC 2，账号 home DC 5 |
| T3 2FA | pass | `SESSION_PASSWORD_NEEDED` → `[T3] password=rejected count=1 state=AwaitingPassword` → `[T3] password=accepted rejected_before=1` |
| T4 过期刷新 | pass | `[T4] expired_import=rpc 400 AUTH_TOKEN_EXPIRED`、`[T4] refreshed=new_token` |
| T5 加密落库后恢复 | pass | `[T5] ciphertext_bytes=809 api_id_set=true restored_read=1 worker_online=true exit=Shutdown status_after=offline` |
| T6 撤销会话 | pass（修复后） | `[T6] exit=Unauthorized detected_after=890.670316ms status=unauthorized audits=1 lease_takeover=ok in_candidates=false` |

真机事件序列全程同时喂给 `tgm_tg::qr::QrLoginState`（spec/08 §3.4 的纯状态机），
序列不合法会使工具失败；T1–T4 运行中没有出现状态机拒绝。

T2 用的是生产 DC 的真实迁移，不是 Test DC。RK-01 关于「需要注册在非默认 DC 的账号」的素材，
用现有测试账号即可取得。

## 二、过程

### T1–T4：`live_user_qr_login`

```bash
TGM_LIVE_QR_EXPIRY_PROBE=1 cargo test -p tgm-tg --test qr_login live_user_qr_login -- --ignored --exact --nocapture
```

- 第一次运行（12:52 起）：T4 探针先导出一代 token 不展示，等 30 秒过期后 `importLoginToken`，
  服务端返回 `400 AUTH_TOKEN_EXPIRED`；重新导出得到不同 token。之后 20 分钟内无人扫码，
  工具以「等待扫码超时」失败退出，这次运行不计入 T1–T3。
- 第二次运行（13:12 起，未开 T4 探针）：token 每约 30 秒换一代，第 7 代被扫码。
  收到 `MigrateTo 2 -> 5`，切到 DC 5 导入成功后进入 `SESSION_PASSWORD_NEEDED`。
  先给错误密码：被拒，状态停在 `AwaitingPassword`，未授权、未退出；再给正确密码：授权完成。
  密码经仓库外文件交付，工具读后即删。
- 之后 `live_user_session_resume` 用新 session 恢复：`is_bot=false home_dc=5`，能读测试频道最近消息。

### T5：`live_t5_session_survives_restart_via_database`

`sqlx::test` 临时库、`app_user` 连接、开发 EnvKek。新 session 经生产路径 `save_session` 加密落库
（密文 809 字节），丢弃客户端后 `load_session` 解密恢复，读到频道消息；再以 `run_identity`
只凭库里的 session 上线为 `online`，停止信号后退出 `Shutdown`、状态回 `offline`。

### T6：`live_t6_revoked_session_marks_identity_unauthorized`

真机暴露了一个生产缺陷，按提案「最小修复在批准范围内」处理。

1. 第一次运行（约 13:30，人已在手机端结束会话）：worker 180 秒内一直是 `online`。
   随后用本次 session 调 `getMe`，13:37:00 仍成功；13:38 起 `account.getAuthorizations` 与 `getMe`
   均返回 `401 AUTH_KEY_UNREGISTERED`。撤销从手机端操作到服务端拒绝 auth key 之间的延迟未查明，
   因此这次运行不作判定。（10-20 的旧 session 文件同样返回 401，它何时失效无法确定。）
2. 13:38 起确认本次 session 已失效。
3. 第二次运行（修复前，session 已确认失效）：worker 180 秒内仍是 `online`，**fail**。
   原因：`connect_with_session` 用失效的 auth key 也能建连并置 `online`；撤销后服务端不再推更新，
   更新流只是静默、不报错，主循环只在 `next_incoming` 出错时识别 `is_unauthorized`，永远等不到。
4. 修复（`crates/tg/src/client.rs`、`crates/app/src/mtproto_worker.rs`）：
   - 新增 `TelegramClient::check_authorized`：一次 `users.getUsers(self)`，只读，不违反 spec/08 §1.4。
   - worker 上线前先校验授权，失效即转 `unauthorized`，不再置 `online`。
   - 运行中每 `auth_check_interval`（默认 60s）校验一次；失效转 `unauthorized`，网络类错误只记日志、下周期再查。
   - 三处 `unauthorized` 出口共用 `mark_unauthorized`（转状态、写审计、释放租约在同一持租约事务内，沿用
     `mark_leased_identity_unauthorized`）。
5. 修复后重跑：0.89 秒内退出 `Unauthorized`，状态 `unauthorized`，`identity.unauthorized` 审计恰好一条，
   租约立即可被接管，身份不再出现在 `list_mtproto_worker_candidates` 中（不自动重试，spec/08 §五）。

T6 结束后，两份测试 session 文件均已确认失效并删除。WP-5 的媒体真机工具之后需要重新扫码。

## 三、未覆盖与局限

- **运行中周期校验这条分支没有真机证据。** 修复后的 T6 是在上线前校验时识别的；
  「在线期间被撤销、下一个 60s 周期识别」需要撤销发生在 worker 在线期间，本次未单独测。
  两条分支调用同一个 `check_authorized` 与 `mark_unauthorized`。
- 修复没有新增 L2：识别依赖 Telegram 返回 401，离线 `sqlx::test` 造不出这一响应。
  错误分类由 `crates/tg/src/client.rs` 现有 L1（`is_unauthorized` 对 `AUTH_KEY_UNREGISTERED` /
  `SESSION_REVOKED` / `FLOOD_WAIT`）覆盖，转状态、审计、释放租约由 `crates/app/tests/mtproto_worker.rs`
  的 `unauthorized_releases_lease_audits_and_is_not_reverted` 覆盖。
- 周期校验每身份每 60 秒一次 `getMe`。身份数量大时的请求量与 `FLOOD_WAIT` 未评估。
- spec/08 §五 第 2 条「通知租户重新授权」仍未做（同 `WP-2-mtproto-worker运行链-2026-10-19.md`）。
- 本次只证明 QR 登录在真机上可行、各分支符合 spec/08 §3.4；QR 登录尚未作为产品功能接入控制面
  （提案片 2，另行提案）。
- 过程说明：T1–T4 运行后，一次误用 `git checkout` 冲掉了 `crates/tg/tests/qr_login.rs` 的未提交改动，
  随后从会话记录按原顺序重放了同一组补丁。重放版本的二进制与真机运行时不是同一份，
  已由完整门禁重新编译、lint 并跑 L1；真机输出以本文记录为准。
