# WP-6：控制台会话签发安全面

## 当前状态

控制台扫码挑战材料、状态迁移、REST 创建/轮询端点，以及控制 Bot 的 `/start` 和确认/拒绝动作解析已落地。本轮已按批准的最小方案补齐真实扫码会话签发。

本轮在不触碰安全面升级边界的前提下，落地了 OIDC 登录回调：服务端生成
`state`、PKCE `code_verifier` / `code_challenge`、nonce；`/auth/oidc/start`
返回带 `S256` 的 Telegram 授权跳转，并用 HttpOnly、短期 cookie 绑定浏览器
state。`/auth/oidc/callback` 通过固定配置端点完成 token exchange，使用固定配置算法
和 JWKS `kid` 完成验签，校验 issuer、audience、azp、exp、sub、nonce 后，复用
`issue_web_session_for_user` 签发会话。失败时清理 state cookie，不写入会话。

## 已批准并实现的改动

### 1. 明确鉴权域写入路径

现有 `web_sessions` 对 `auth_lookup` 只有 `FOR SELECT` 的 RLS 策略，虽然角色拥有表级 DML，写入仍会被 RLS 拒绝。本轮采用新增迁移
`migrations/0015_web_session_insert_policy.sql` 的最小方案：只增加
`auth_lookup` 的 `FOR INSERT` 策略，并要求 `user_id` 等于事务内显式设置的
`app.auth_user_id`；不开放 `app_user`，不使用 `SECURITY DEFINER`，不新增角色。

### 2. 收口会话审计字段

`web_sessions` 当前没有登录方式列，但接口契约要求会话列表能区分 OIDC 与控制 Bot 扫码登录。需要先决定是新增列/迁移，还是把登录方式放入既有审计关联；前者命中表结构升级，后者需要明确可证明的关联键，不能靠时间或日志猜测。

### 3. 两条路径的会话测试

本轮已实现并验证：

- 控制 Bot 确认后只给绑定的 Telegram 用户签发会话；
- OIDC callback 只接受已验证成员；
- 会话查询遵守 `auth_lookup` 自查策略；
- 成员移除后会话立即失效；
- R17 钓鱼路径、多租户不猜租户、R20 `alg` 拒绝、R21 登录材料一次性消费与回调重放拒绝反向测试通过；
- token exchange 表单、RS256/JWKS 正向验签和篡改 `alg` 的拒绝测试通过。
- provider 失败路径已覆盖：token endpoint 的 429/503 映射为
  `OIDC_PROVIDER_UNAVAILABLE`，非法或超大 JWKS、未知 `kid` 同样拒绝并返回 503。
- 本地 stub 成功回调已验证：只接受唯一租户成员，写入 `web_sessions`，签发 session/CSRF
  cookie，并在同一登录材料重放时于网络调用前拒绝。

### 4. OIDC 回调实现边界

- token endpoint 使用 `application/x-www-form-urlencoded`，包含 client id、secret、redirect URI 与 PKCE verifier；
- token 与 JWKS HTTP client 禁止跟随重定向，并限制连接/响应体大小；
- JWT header 的 `alg` 必须匹配配置，`kid` 必须唯一命中 JWKS；
- JWK 的 `use`、`key_ops`、`alg` 与配置不一致时拒绝；
- 多 audience 必须带匹配的 `azp`；
- `web_sessions` 仍没有登录方式列，相关结构升级继续保留在升级范围内。

## 升级原因

命中 `AGENTS.md` §五 的安全面升级条件：需要改动 RLS 路径。用户已批准上述最小实现；未新增表结构、角色或 `SECURITY DEFINER`，也未修改已应用迁移。

## 当前验证

- `cargo fmt --all -- --check` 通过。
- `git diff --check` 通过。
- `SQLX_OFFLINE=true cargo check --workspace` 通过。
- `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test -p tgm-core --lib` 通过（含 OIDC 协议层测试）。
- `cargo test -p tgm-app --lib` 通过（含 OIDC 起始/回调安全壳测试，以及缺少 `code` 时清理 state cookie 的回归测试）。
- `scripts/ai-driver/ai-gates.sh --task-only --from test` 通过；任务测试、数据库集成测试、隔离测试与 pipeline 测试均通过。
- `scripts/ai-driver/ai-gates.sh --trusted-db-only --from audit-rls` 通过；RLS 审计与 double-pass 均通过。
- 任务门禁本轮包含 `tgm-app` 38 项单元测试、控制面 22 项集成测试及 workspace 数据库/隔离/pipeline 测试，全部通过。
- `SQLX_OFFLINE=true cargo run --quiet --bin tgm -- openapi --check` 通过。
- `cargo test -p tgm-app --test control oidc_callback_issues_session_from_verified_member -- --exact` 通过；测试使用本地 token/JWKS stub 和受限 `auth_lookup` 连接池。
- OIDC provider 失败路径单测通过：429/503、非法或超大 JWKS、未知 `kid`。
- 新增迁移 `migrations/0015_web_session_insert_policy.sql`；OIDC 真实联调、Telegram 真机和 BotFather 凭据仍未执行。

[report]
task: WP-6-控制台会话签发安全面
path: escalated
gates: pass
escalation: docs/plan/proposals/WP-6-控制台会话签发安全面.md
