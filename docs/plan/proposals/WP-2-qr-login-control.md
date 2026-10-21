# WP-2-qr-login-control：QR 登录接入控制面（片 2）

状态：按 D1–D4 推荐方案、D5 只交付 API 实现（2026-10-21），人已批准提交（2026-10-21）。
L1/L2、完整门禁与经控制面端点的真机验收（扫码 + 2FA + worker 上线，pass）见
`docs/plan/evidence/WP-2-qr-login-control-2026-10-21.md`。

## 任务与判据

任务 id：`WP-2-qr-login-control`。承接 `docs/plan/proposals/WP-2-qr-login-live.md` 的片 2。

判据出处：
- `docs/design/spec/07-接口契约.md` §3.3：`POST /api/v1/identities/login/qr`、
  `GET /api/v1/identities/login/qr/{id}`、`POST /api/v1/identities/login/password`；
  QR 轮询是唯一必须限流的读端点，`login_id` 用随机字节。
- `docs/design/spec/08-账号接入.md` §3.1–§3.4、§1.3（登录完成后的那一次落库属于 QR 调用链，尚未接入）、§五。
- `docs/design/spec/06-合规与凭据责任.md` §2.2 与 `docs/design/plan/00-里程碑与工期.md` §4.1
  「同意文本与落库进 WP-2（首次登录即需）」：pro 档代持授权必须先于凭据录入。
- `docs/design/spec/02-总体架构.md` §6.3 与现有 `CredentialViaWebOnly`：凭据只经 Web 提交。
- `docs/design/spec/05-安全与租户隔离.md` §七：control-plane 只有 `kms:Encrypt`。

按 `AGENTS.md` §五升级：第 3 条（凭据处理、可能触及 GRANT/RLS）、可能的第 1 条（迁移），
以及新增依赖。

## 现状（2026-10-21 核对代码）

- 真机可行性已证（片 1，`docs/plan/evidence/WP-2-qr-login-live-2026-10-21.md`）。
  QR 驱动逻辑目前只在测试工具 `crates/tg/tests/qr_login.rs` 里，生产代码没有。
- `login_sessions`（0007）：`id` 是自增 BIGINT（可枚举，与 spec/07 §3.3「随机字节」冲突），
  `login_token` 是明文 BYTEA（bearer 凭据），没有 `identity_id`、代次、密码态的关联列。
  全仓库没有任何代码读写它。RLS 三件套与 GRANT 已在。
- `tenant_consents`（0007）已有 `credential_custody` 类型，`app_user` 只有 `SELECT, INSERT`，没有代码写入。
- `identity_secrets.api_id` / `api_hash_ciphertext` 列已有；`api_hash` 没有加解密代码、没有 AAD 前缀。
  mtproto-worker 只用 `api_id`（握手），`api_hash` 只在 `exportLoginToken` 用到。
- control-plane 已有凭据录入先例 `POST /api/v1/identities/bot`：仅 Web、owner/admin、
  wrap-only KEK、事务内建身份 + 写密文 + 审计。
- 没有限流设施。形态 A 单实例（ADR-0025）。
- 没有二维码生成依赖。

## 关键设计取舍（需要你决定）

### D1 登录过程中的 MTProto 连接与 login_token 放哪

QR 登录要在几分钟内持有一条 grammers 连接（同一 auth key 才能在扫码后导入），跨多次 HTTP 轮询。

- **方案 A（推荐）**：control-plane 进程内持有登录中的连接，按随机 `login_id` 索引，
  10 分钟上限、完成或过期即丢弃。`login_token` 只在内存里，**不落库**。
  - 优点：bearer 凭据不进数据库；无需迁移；无需 worker 侧队列。
  - 代价：进程重启丢失进行中的登录（用户重扫即可）；只在单实例下成立 ——
    上多实例前要换成粘性路由或 DB 方案（留 `ponytail:` 注释与台账）。
  - `login_sessions` 表本片不使用，保持空表。spec/08 §3.4「更新 `login_sessions.login_token`」
    与 spec/03 的说明改写为实现现状（C/B 级变更，记录在证据里）。
- 方案 B：照 spec 落 `login_sessions`。需要迁移：随机 id 列、`login_token` 改为密文、加 `identity_id`
  与代次；而且 control-plane 没有 Decrypt 权限，读不回加密的 token，只能明文存 —— 这正是要避免的。
  不推荐。

### D2 `api_hash` 是否落库

- **推荐：不落库**。租户每次发起 QR 登录（首次或重新授权）时在请求里提交 `api_id` + `api_hash`，
  只在内存里用于 `exportLoginToken`，登录结束即丢弃。`api_id` 照现状写入 `identity_secrets.api_id`
  （worker 需要它），`api_hash_ciphertext` 保持 NULL。
  - 理由：control-plane 解不开密文，存了也用不上；worker 不需要 `api_hash`。少存一份凭据就少一份泄漏面。
  - 代价：重新授权时租户要再填一次 `api_hash`。
- 备选：wrap-only 加密落库，留给将来的组件解密。当前没有消费方，属 YAGNI。

### D3 二维码交付形态与新依赖

spec/07 写的是 `qr_png_base64`。

- **推荐**：新增 `qrcode = "=0.14.1"`（MIT/Apache-2.0，kennytm/qrcode-rust），
  `default-features = false, features = ["svg"]`，不引入 `image`；
  响应字段改为 `qr_svg`（SVG 字符串），契约随之修改。
- 备选 1：保留 PNG，开 `image` 特性（依赖树显著变大）。
- 备选 2：只返回 `tg://login?token=…` 由前端渲染。需要前端 JS 二维码库，本仓控制台无构建链，不推荐。

无论哪种，响应都含 bearer 凭据：响应头 `Cache-Control: no-store`，不进日志，不经 Bot 入口。

### D4 授权同意与套餐门槛

- 仅 `plan = 'pro'` 的租户可发起（free 档不碰 MTProto，spec/01 §六）；否则 403。
- 发起前必须有该租户当前版本 `credential_custody` 的同意记录。请求体带 `consent_text_version`，
  服务端只接受当前版本常量；首次发起时同一请求内插入 `tenant_consents`（记 user_id、ip、user_agent）。
  授权文本正文属于 WP-8 的法务工作，本片只放占位文本与版本号 `custody-v0`，证据里注明「占位」。

## 片 2 范围（D1–D4 按推荐时）

不新增迁移，不改 RLS / GRANT / 审计断言 / CI。

1. `crates/tg`：把测试工具里的驱动逻辑提成生产代码 `QrLoginDriver`（export → 轮询 →
   `MigrateTo` 切 DC → 可选 2FA），状态推进走现有 `QrLoginState`。片 1 的测试工具改为调用它，
   真机路径不重复两份。
2. `crates/app/src/control.rs`：三个端点（仅 Web、owner/admin、pro 档、已同意）。
   - `POST /login/qr` `{api_id, api_hash, consent_text_version, identity_id?}` → `{login_id, qr_svg, expires_at}`。
     带 `identity_id` 时是对已有 MTProto 身份重新授权。
   - `GET /login/qr/{login_id}` → `pending | awaiting_password | confirmed | expired | failed`；
     pending 时附新一代 `qr_svg`（token 轮换）。`login_id` 绑定发起者的租户与用户，跨租户、跨用户一律 404。
   - `POST /login/password` `{login_id, password}`：错误密码返回可重试，状态停在 `awaiting_password`。
   - 限流（进程内）：每 `login_id` 轮询不快于每 2 秒一次；每租户同时进行中的登录不超过 2 个；
     每租户每小时发起不超过 10 次；超限 429。数字是首发占位，留 `ponytail:` 注释。
3. `crates/db`：`create_mtproto_identity`（照 `create_bot_identity`：advisory lock 按 `tg_user_id` 去重、
   wrap-only 写 session 密文、写 `api_id`、`identity.create` 审计）；
   `reauthorize_mtproto_identity`（session 覆盖、`unauthorized → offline`、`identity.reauthorize` 审计，
   必须与登录到的 `tg_user_id` 一致，否则拒绝 —— 防止把别的账号接到老身份上）；
   `insert_credential_custody_consent`。
4. OpenAPI 契约更新、`tgm openapi --check` 通过；spec/07 §3.3、spec/08 §3.4、§1.3 按实现改写。
5. 测试：
   - L1：限流、`login_id` 随机性与长度、状态映射。
   - L2：重新授权的 `tg_user_id` 不一致被拒；free 档 403；未同意 403；Bot 入口提交 403；
     跨租户 `login_id` 404；同意记录写入且不可改（R22 已覆盖不可改）。
     以 `app_user` 受限角色连接。
   - 真机（ignored）：经控制面端点完成一次 QR + 2FA 登录，worker 用落库的 session 上线。需要你再扫一次码。
6. 证据 `docs/plan/evidence/WP-2-qr-login-control-<日期>.md`。

## 不在范围

- 租户通知（spec/08 §五第 2 条）：没有通知通道。
- 授权文本正文、留存与删除（WP-8）。
- 控制台身份页的 QR 登录界面：先交付 API；页面另作一片，或你决定并入本片。
- 多实例下的登录连接共享。

## 需要你决定的事

1. D1：登录连接与 `login_token` 只放 control-plane 内存、本片不用 `login_sessions`（推荐），还是照 spec 落库。
2. D2：`api_hash` 不落库、每次登录提交（推荐），还是 wrap-only 加密落库。
3. D3：新增依赖 `qrcode = "=0.14.1"`（仅 svg 特性），契约字段改为 `qr_svg`（推荐）；或其他形态。
4. D4：pro 档门槛与 `credential_custody` 占位文本 `custody-v0` 是否可行。
5. 控制台 QR 页面是否并入本片。
