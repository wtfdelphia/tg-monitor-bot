# WP-7：认证会话管理安全面

## 当前状态

本轮补齐了 `GET /auth/me`，并完成了 WP-7 控制台六页中认证会话管理以外的页面交互：
响应只返回已有 `AuthContext` 中的 `user_id`、`tenant_id` 与角色，不读取前端传入的租户
参数，也不暴露 session、CSRF、IP 或 User-Agent。对应控制面集成测试已使用受限
`auth_lookup` 连接池验证有效会话路径。

经批准后，本轮已实现 `POST /auth/logout`。认证中间件把真实 session id 注入
`AuthContext`，repo 在事务内设置 `app.session_id` 后删除当前会话，RLS 只允许
`auth_lookup` 删除该 session；成功响应为 `204 No Content`，并清理两个认证 Cookie。

该登出切片完成后，`GET /auth/sessions` 与 `DELETE /auth/sessions/{id}` 已在后续批准的
任务中实现，`login_method` 也由新增迁移补齐。后续范围与验收状态见
`WP-7-会话列表与远程踢下线.md`；本报告仍只代表当前会话登出切片。

## 已批准并实现

### 1. 当前会话登出

已新增一个只允许删除当前会话的 `auth_lookup` RLS 策略，谓词继续绑定事务内的
`app.session_id`：

```sql
CREATE POLICY session_self_delete ON web_sessions FOR DELETE TO auth_lookup
USING (id = current_setting('app.session_id')::bytea);
```

repo 方法保持现有鉴权查询形状：事务内设置 `app.session_id`，按 session id
删除，影响行数不作为跨租户可观测信号。HTTP 成功响应为 `204 No Content`，同时清理
`tgm_session` 与 `tgm_csrf` cookie；CSRF 继续由现有认证中间件校验。

## 未实现范围

当前两条登录路径都写入同一张 `web_sessions`，但表中没有 `login_method`。不能按时间、
User-Agent 或审计日志猜测来源。后续若实现会话列表与指定会话撤销，应先新增明确的列与
迁移，再补齐对应 RLS、GRANT 与反向测试。

## 测试与契约

本轮已补齐当前会话登出的测试：

- logout 正确 CSRF 删除当前 session，并清理两个 cookie；
- CSRF 缺失或错误时不删除 session；
- auth_lookup 只能删除 `app.session_id` 对应的 session；
- 未设置 `app.session_id` 时删除失败闭锁；
- `app_user` 无权删除 `web_sessions`；
- 登出后复用原 Cookie 访问 `/auth/me` 返回 `401`；
- 清理 Cookie 保留 `Path`、`HttpOnly`、`Secure` 与 `SameSite` 属性；
- OpenAPI 与 `openapi.json` 同步，`openapi --check` 通过。

会话列表与指定会话撤销的隔离测试仍待对应实现。

## 升级原因

本任务命中仓库根 `AGENTS.md` §五 的升级条件：需要新增迁移、修改
`web_sessions` 的 RLS/GRANT 安全面，并补充安全隔离断言。批准前不直接写
`0016` 迁移、不放宽现有策略、不使用 `SECURITY DEFINER`，也不通过 app_user
绕过鉴权域。

## 当前验证

- `cargo fmt --all -- --check`、`git diff --check`、`SQLX_OFFLINE=true cargo check --workspace`
  与 `SQLX_OFFLINE=true cargo clippy --all-targets --all-features -- -D warnings` 通过。
- 当前工作树的 `scripts/ai-driver/ai-gates.sh --task-only --from test` 通过，包含 workspace
  测试、控制面测试、隔离测试、pipeline、delivery、events、lease 与 SQLx 测试缓存检查。
- 可信数据库卡口通过：迁移幂等、`audit-rls` 与 `scripts/audit-double-pass.sh` 均通过；
  当前生产库最大迁移版本为 16，`0016_web_session_delete_policy` 已应用。
- `SQLX_OFFLINE=true cargo run --quiet --bin tgm -- openapi --check` 与
  `cargo sqlx prepare --workspace --check` 通过。

[report]
task: WP-7-认证会话管理安全面
path: ok
gates: pass
escalation: ->
