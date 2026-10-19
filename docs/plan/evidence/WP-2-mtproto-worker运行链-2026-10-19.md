# WP-2 mtproto-worker 运行链

记录日期：2026-10-19

任务 id：`WP-2-mtproto-worker`

判据出处：`docs/design/spec/08-账号接入.md` §1.3（落库时机）、§1.4（只读纪律）、
§4.2–§4.3（租约与 fencing token）、§五（身份状态机）；`docs/design/plan/00-里程碑与工期.md`
§4.3 WP-2（租约抢占行为可复现）；`docs/plan/编码实施前置待办.md` §8。

## 一、范围

- `tgm serve --role mtproto-worker` 接入运行链（`bins/tgm/src/main.rs`）。
  `worker_id` 取主机名加进程号，重启后不会把旧进程的租约当成自己的续上。
- `crates/app/src/mtproto_worker.rs`：每 15s 扫描可接管身份，每个身份一个任务：
  领租约 → `load_session` 解密恢复 → `connect_with_session` 建连 → 置 `online`
  → 收消息落 `canonical_events`。每 10s 续租，每 60s 落一次 session，正常退出前再落一次。
  异常退出的身份等 30s 再试。
- `crates/tg/src/client.rs`：`next_incoming` 只返回别人发的新消息与编辑；自己发出的、
  服务消息、空消息跳过。媒体类型名与 Bot 路径一致（PHOTO / VIDEO / VOICE / …），
  规则的 `media_type` 条件对两种来源一视同仁。`is_unauthorized` 识别 401、
  `AUTH_KEY_UNREGISTERED`、`SESSION_REVOKED`、`USER_DEACTIVATED*`。
- `crates/db/src/repo.rs`：`LeaseGuard` 与 `lease_is_held`（事务内 `FOR SHARE` 校验
  worker、token 与未过期）；`insert_canonical_event_leased`、`set_leased_identity_status`、
  `mark_leased_identity_unauthorized`、`list_mtproto_worker_candidates`。
  `insert_canonical_event` 拆出连接级实现，Bot 路径行为不变。
- `crates/db/src/credentials.rs`：`store_identity_secret_leased`，session 写入事务内校验租约。

不涉及迁移、RLS、GRANT、审计断言、CI 或门禁脚本。所有新 SQL 用运行期 `query`/`query_as`，
不新增 `.sqlx/` 条目；`insert_canonical_event` 的两条 `query!` 文本未改。

## 二、设计取舍

- **所有写操作都带 token**（spec/08 §4.3 的前提）：事件、session、身份状态三类写入都在
  同一事务里先 `FOR SHARE` 锁住租约行再写。接管方的 `UPDATE account_leases` 要等本事务
  提交，提交后旧 token 不再匹配。续租失败直接退出该身份，不重试、不重连。
- **`unauthorized` 不自动恢复**：转入时同一事务内释放租约（只置 `expires_at`，保留行与 token）
  并写 `identity.unauthorized` 审计（`source = 'system'`）。`set_leased_identity_status`
  不会把 `banned` / `unauthorized` 改回去；候选列表也排除这两个状态。
  租户通知（spec/08 §五 第 2 条）未做，没有通知通道。
- **`edit_version` 取 `edit_date`**：单调但不连续。`catch_up` 补齐会重投同一次编辑，
  Bot 路径的 `MAX + 1` 会把重投写成新版本；`edit_date` 让重投撞 `uq_canonical` 去重。
- **有媒体时 MTProto 的 `message` 落 `caption`**，与 Bot API 的 caption 对应。
- **候选只含租户自有身份**：平台演示身份（`tenant_id IS NULL`）的凭据不属于任何租户，
  现有读取路径按租户连接，接不上。演示源的采集另议。
- 沿用 `TGM_KEK_BACKEND` 等既有配置，不新增环境变量；`api_id` 读 `identity_secrets.api_id`
  （ADR-0013）。`api_hash` 当前握手不需要，`api_hash_ciphertext` 仍未使用。

## 三、验证记录

- `cargo test -p tgm-tg --lib`：33 项通过。新增 4 项：频道消息用 Bot API 形式的 chat id；
  自己发出的与空消息不采集；文档属性映射到 VOICE / DOCUMENT 并带文件名、大小、MIME；
  401 类错误判为失效、`FLOOD_WAIT` 不判。
- `cargo test -p tgm-app --test mtproto_worker`：8 项以受限角色 `app_user` 连接通过：
  1. 旧 token 写不进事件，新持有者写得进（token 递增）；
  2. 同一消息重投去重，新的一次编辑另落一行；
  3. 旧 token 不能覆盖新持有者的 session，返回 `LeaseLost`，库里保留新快照；
  4. 失效后身份为 `unauthorized`、审计记录前态、租约立即可接管且 token 递增，
     新持有者不能把状态改回 `online`；
  5. 旧 token 不能标记失效、不能改状态；
  6. 候选必须有 session 与 `api_id`、排除 `banned`、不跨租户；
  7. 没有 session 的身份返回 `NoSession` 并主动释放租约，另一 worker 立即接管；
     租约被占时返回 `LeaseBusy`；
  8. 有媒体时文本落 `caption`，纯文本落 `text`。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过。

## 四、反证

把 `lease_is_held` 的条件换成恒真（去掉 worker、token 与过期判断），重跑上面 8 项：
1、3、5 三项报红，其余通过；随后还原。三项红正是依赖 fencing 的那三条。

## 五、门禁

2026-10-19 本地前台跑 `scripts/ai-driver/ai-gates.sh`（库卡口连 Compose 栈 `kms-kek`），rc=0。
fmt、clippy、deny、newtype、offline-build、sqlx-check、openapi-check、migrate、
migrate-idempotent（19 个迁移文件）、audit-rls、test、double-pass、driver-regressions 全部通过；
test 卡口合计 293 项通过、0 失败。double-pass 逐条破坏 MAIN、A1–A9、C1/C2/C4/C5/C6，
各自恰好红对应一条（A2b 连带 A1 为已登记冗余）。最终裁决仍以 CI 为准。

## 六、未验证

- 真机：授权账号收消息、断线后 `catch_up` 补齐、`unauthorized` 真实触发、
  60s 落库后进程重启恢复（`编码实施前置待办.md` §3、§4）。
- 真实 `KmsKek` 下运行期 `Decrypt`：`load_session` 已有运行期调用方，但没有在真实 KMS 下跑过。
- spec/08 §4.5 混沌清单一条未跑；首发形态 A 固定单实例，结论不变。
- 租户通知、QR 登录完成后的首次落库（属于 QR 调用链）、媒体下载与中转（spec/02 §4.2）。
- `delivery` 仍未接入：发送通道与规格冲突，见 `docs/plan/proposals/WP-5-delivery-sender-channel.md`。
