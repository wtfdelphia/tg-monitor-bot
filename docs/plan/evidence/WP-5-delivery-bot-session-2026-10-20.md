# WP-5-delivery-bot-session：Bot session 落库后的重启风暴（2026-10-20）

决策：`docs/design/decisions/ADR-0034-投递Bot的session加密落库.md`。
提案：`docs/plan/proposals/WP-5-delivery-bot-session.md`。
前序实测（session 不落库时的限流）：`docs/plan/evidence/WP-5-delivery常驻长跑-2026-10-19.md` §二。

## 一、真机重启风暴

工具 `bins/tgm/tests/delivery_soak.rs`（`#[ignore]`），新增判据 S5：只有第一次启动用 Token 登录，
之后每次重启都复用落库的 session。参数比前序长跑激进：重启间隔 60 秒（前序默认 600 秒），
SIGTERM 与 SIGKILL 交替。同一个测试 Bot、同一个测试频道。

```text
TGM_SOAK_MINUTES=8 TGM_SOAK_RESTART_SECS=60 TGM_SOAK_TASK_SECS=15 \
  cargo test -p tgm --test delivery_soak -- --ignored --nocapture

[soak] minutes=8 tasks=32 done=32 distinct_message_ids=32 dead=0 starts=8 logins=8
       token_logins=1 login_flood_waits=0 sigterm=5 sigkill=3 slowest_sigterm_exit=113.692771ms
       failures=[] identity_status=offline unauthorized_audits=0
test result: ok. 1 passed; 0 failed; finished in 483.35s
```

| 判据 | 结果 |
|---|---|
| S1 不丢不重 | 32 条全部 DONE，32 个不同的 `telegram_message_id` |
| S2 无死信 | `dead=0`，失败日志为空 |
| S3 SIGTERM 优雅退出 | 5 次，最慢 114ms，均以 0 退出 |
| S4 身份未被误标 | `identity_status=offline`，`identity.unauthorized` 审计 0 条 |
| S5 重启复用 session | 8 次启动 8 次登录，其中 Token 登录 1 次、复用 session 7 次；`FLOOD_WAIT` 0 次 |

对照：前序实测在 session 不落库时，几分钟内第 4 次左右 `bot_sign_in` 就开始被限流 66–305 秒。
本次一分钟一次重启、7 次重启，没有一次调 `auth.importBotAuthorization`。SIGKILL 之后也复用成功：
session 在 Token 登录成功时立即写回，不依赖退出路径。

日志与输出里没有 Token 与 `api_hash`（按值扫描过）。

频道人工核对（`tgm-soak #1..#32` 每条只出现一次）：测试环境提供方于 2026-10-20 确认通过。

## 二、L2 测试（受限角色 `app_user`）

`crates/db/tests/credentials.rs` 新增三条：

- `bot_session_roundtrips_and_is_cleared_when_token_is_replaced`：session 密文不含明文魔数；
  覆盖 Token 后两列清空；拿 Token 被换之前的读数写回 session 被拒。
- `tampered_bot_session_falls_back_and_can_be_cleared`：把 Token 密文抄进 session 列解不开
  （同 DEK、AAD 不同），Token 照常读出；两列只填一列被 CHECK 拒绝。
- `bot_session_is_tenant_scoped`：别的租户读不到、写不进、清不掉。

反证：

- 去掉 `UPSERT_SECRET` 里清空 session 的两行，第一条在「覆盖 Token 后两列清空」处变红。
- 把写回 session 的 `s.dek_ciphertext = $5` 换成恒真条件，第一条在「旧读数写回被拒」处变红。

`crates/core/src/credential.rs` 新增 `token_and_bot_session_ciphertexts_are_not_interchangeable`。

## 三、卡口

完整 `scripts/ai-driver/ai-gates.sh`（基于 `codex/WP-5-delivery-soak-tool` `327ce45`）：rc=0，
测试 306 passed / 0 failed / 5 ignored，`migrate-idempotent 通过`、`double-pass 通过`、
`已执行的全部本地卡口通过`。

## 四、重置 Token 注销已有 auth key（V4，2026-10-20）

回答 ADR-0034 代价节的未验证项。工具 `crates/app/tests/delivery_live.rs` 的 V4a / V4b（`#[ignore]`），
两段之间由测试环境提供方在 BotFather 重置 Token。session 文件放仓库外、权限 0600，V4b 读出即删。

第一次尝试没有结论：长跑跑在 `sqlx::test` 临时库里，重置前的 session 随库删除。第二次按
「登录并留存 session → 重置 → 用旧 session 复用」重做：

```text
[V4a] session 已存，重置前复用成功 bot_id=8869608883；现在去 BotFather 重置 Token 再跑 V4b
        ── 测试环境提供方在 BotFather 重置 Token ──
[V4b] 重置后旧 session 建连失败 unauthorized=true error=Telegram 调用失败: request error:
      rpc error 401: SESSION_REVOKED caused by users.getUsers
[V4b] 结论：重置 Token 注销了已有 auth key
```

结论：重置 Token 时，Telegram 同时注销该 Bot 已签发的 auth key。落库的 session 不会比 Token 活得更久，
「重置 Token 即失效」对 session 同样成立。样本是一个 Bot、一次重置。

运行链上的走向：`SESSION_REVOKED` 已归入 `TelegramClientError::is_unauthorized`
（`crates/tg/src/client.rs` 的单元测试覆盖）→ `resume_from_stored` 清掉 session、退回 Token 登录 →
旧 Token 被 `auth.importBotAuthorization` 拒绝 → 身份转 `unauthorized` 并写审计（V3 已真机验证）。
control-plane 重写 Token 时清空 session 仍保留，作为第二道。

## 五、未验证

- 真实 `KmsKek` 下写回 session 的那次 unwrap（本地用 `EnvKek`）。
- 长于 8 分钟的风暴、多个 Bot 同时重启。
