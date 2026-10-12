# WP-7：会话列表与远程踢下线

## 当前状态

当前会话登出已经完成：`POST /auth/logout` 只删除当前 `web_sessions` 行，
并由 `app.session_id` 绑定 RLS。现有 `GET /auth/me` 也只从认证上下文返回当前身份。

本切片按接口契约实现：

- `GET /auth/sessions` 返回当前用户的活跃会话列表；
- `DELETE /auth/sessions/{id}` 删除当前用户指定的会话，包含当前会话之外的会话；
- 列表返回 IP、User-Agent、登录方式与创建时间。

用户已批准本提案。迁移 `0017_web_session_management.sql`、两条接口与登录来源写入
已经实现，并补齐受限角色和真实 Cookie/CSRF 的反向测试。控制台顶部已接入会话管理
面板和退出登录，支持刷新、收起与撤销本人会话；普通成员不受业务写权限限制。
2026-10-12 在线依赖审计与包含最终前端实现的完整本地门禁通过，原网络阻塞已解除。
本报告只收口会话管理切片，不代表整个 WP-7 工作包完成。

## 需要批准的改动

以下范围已获批准并实现；策略的缺省拒绝形状和管理引用在实现时按实际认证链路收口。

### 1. 记录登录方式

新增 `migrations/0017_web_session_management.sql`，测试迁移以符号链接复用同一文件，
为 `web_sessions` 增加：

```sql
login_method TEXT NOT NULL DEFAULT 'unknown'
  CHECK (login_method IN ('oidc', 'bot_qr', 'unknown'))
```

迁移使用 `unknown` 兼容已经存在的会话；迁移后的新会话必须由调用路径明确写入：

- Telegram OIDC 回调写入 `oidc`；
- 控制 Bot 扫码确认写入 `bot_qr`。

不按时间、User-Agent 或 `audit_logs` 反推历史会话来源。登录方式值只进入会话列表
响应，不进入日志，也不改变 session id 的保密纪律。

### 2. 扩展鉴权域 RLS

保留当前会话的策略范围，并新增限定 `auth_lookup` 的用户范围策略：

```sql
CREATE POLICY session_owner_list ON web_sessions FOR SELECT TO auth_lookup
  USING (user_id = NULLIF(current_setting('app.auth_user_id', true), '')::bigint);

CREATE POLICY session_owner_delete ON web_sessions FOR DELETE TO auth_lookup
  USING (user_id = NULLIF(current_setting('app.auth_user_id', true), '')::bigint);
```

当前会话 SELECT/DELETE 策略同样使用 `NULLIF(current_setting(..., true), '')`。
PERMISSIVE 策略按 OR 组合：登录前只知道 session id，会话管理只绑定已验证用户；
未设置的另一分支不能抛错。两组上下文都缺失或事务结束后为空串时闭锁为零行。
此变更仅写入新增迁移，没有修改 `0010`、`0015` 或 `0016`。

repo 查询在事务内设置 `app.auth_user_id`，其值只来自已验证的 `AuthContext`；不接受
URL、查询参数或请求体传入的 user id。指定会话撤销按影响行数返回统一成功结果，不把
“不存在”“他人会话”与“已删除”区分给调用方，避免形成枚举信号。

### 3. HTTP 与 OpenAPI

- `GET /auth/sessions`：只返回当前用户的未过期会话，字段包括非登录凭据的会话引用、
  IP、User-Agent、登录方式、创建时间、过期时间与是否为当前会话；不返回租户筛选参数。
- `DELETE /auth/sessions/{id}`：只接受固定长度的 URL-safe session id，复用现有认证
  中间件与 CSRF 校验，成功返回 `204 No Content`。
- `POST /auth/logout` 继续走当前会话专用删除路径，不改为依赖请求参数。
- 同步公共 OpenAPI、管理 OpenAPI 与契约检查；不新增独立角色，不使用
  `SECURITY DEFINER`。

管理引用为 `base64url(sha256(web_sessions.id))`，列表与撤销 URL 不包含登录 Cookie
中的原始 session id。引用不能用来登录；repo 同时按已验证 user id 和引用删除。
撤销当前会话会清理认证 Cookie，撤销其他会话不清理当前 Cookie。管理响应为 `no-store`。

## 验收判据

- 迁移自身幂等，测试迁移镜像一致，已应用迁移文件不被修改。
- OIDC 与控制 Bot 新签发会话分别带有正确的 `login_method`；历史会话安全显示为
  `unknown`。
- 用户跨多个租户查询列表时只能看到自己的会话；其他用户的会话不可见。
- 未设置 `app.auth_user_id` 时列表与指定撤销均 fail-closed。
- 指定撤销不能删除其他用户的会话；本人跨租户会话可撤销，删除不存在或无权会话不泄露存在性。
- 当前会话登出、成员移除立即失效与现有 CSRF 行为保持不变。
- 控制台认证成功后才能操作会话管理与退出登录；本人会话写操作携带 CSRF，
  不复用业务管理员限制。撤销当前会话或退出登录成功后才跳转，失败保留页面并恢复按钮。
- OpenAPI、SQLx prepare、workspace 测试、隔离反向测试、`audit-rls` 与 double-pass
  全部通过。

## 升级原因

本任务命中仓库根 `AGENTS.md` §五 的升级条件：需要新增迁移，并改动
`web_sessions` 的 RLS 安全面与鉴权域查询形态。用户批准后实现上述范围；没有修改
已应用迁移、角色与 GRANT，没有放宽 `app_user` 权限。

## 当前验证

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-7、
`docs/design/spec/07-接口契约.md` §1.4、§3.1，以及本提案的验收判据。

- 当前定向测试通过：跨租户本人列表、远程撤销、无上下文与同一连接的事务上下文回收、
  业务角色拒绝、非法引用、CSRF 拒绝、统一响应、原 Cookie 失效与管理引用无法登录。
- OIDC 与控制 Bot 签发分别验证 `oidc` 和 `bot_qr`；历史表形状上迁移重跑保留 Cookie/CSRF，
  来源为 `unknown`，非法来源受 CHECK 约束拒绝。
- `fmt`、`check`、`clippy -D warnings`、OpenAPI 导出与检查通过。数据库最大迁移版本为 17，
  迁移幂等、SQLx 缓存检查通过。
- `cargo sqlx prepare --workspace` 重建缓存后，
  `scripts/ai-driver/ai-gates.sh --from sqlx-check` 串行通过 SQLx 检查、迁移幂等、workspace
  测试（含 doctest）、`audit-rls` 与 double-pass。离线 workspace 全目标构建和最终 clippy
  也通过。此前缓存重建与 doctest 并发造成的缓存缺失已经在串行重跑中排除。
- 2026-10-11 完整 `scripts/ai-driver/ai-gates.sh` 在 `deny` 处失败：RustSec 数据库获取时 SSH 连接被
  远端关闭。已尝试进程级 HTTPS、直接连接、现有本机代理和 SSH 443，均发生 TLS 超时或
  连接关闭；没有修改门禁、依赖白名单或全局 Git 配置。
- 当时 `cargo deny --offline check` 使用本地 RustSec 缓存通过，缓存提交为
  `7eebec69c352c7191b1f13eb95dd510eeca5d1de`（2026-10-09）。缓存结果不能替代在线更新，
  因此当时没有声明全量门禁通过。
- 2026-10-12 普通 `cargo deny check` 在线获取 RustSec 数据库成功；完整
  `scripts/ai-driver/ai-gates.sh` 在后端基线和最终前端实现上分别通过。
  最终日志为 `/tmp/tgm-wp7-console-final-gates-20261012.log`，包含 workspace 测试、
  迁移幂等、SQLx 检查、`audit-rls` 与 double-pass；double-pass 已恢复数据库快照。
  门禁按现有 CI 约定跳过 OpenAPI，另行执行
  `SQLX_OFFLINE=true cargo run --quiet --bin tgm -- openapi --check` 通过。
- 新增 `crates/app/tests/console_sessions.js`，由 Rust 单元测试把实际页面内嵌脚本交给
  Node 标准库执行。覆盖 VIEWER/MEMBER 的本人会话操作、Cookie/CSRF 请求形状、
  登录来源与 HTML 转义、取消确认、失败恢复、远程撤销刷新、当前撤销与退出登录、
  空列表、刷新收起，以及认证失败时不绑定操作；页面 HTML 回归测试也通过。
- 仅在内存中分别加入业务写权限限制、去掉 CSRF、去掉 User-Agent 转义、让退出失败
  也跳转，交互测试均报红；这些反证变更没有写入仓库。

上述判据的本地检查已通过，没有修改 `scripts/`、CI 或审计断言来绕过失败，也未提交
或创建 PR。本地全绿不是 CI 合并裁决。交互检查使用受控 DOM/fetch，不替代真实浏览器
的布局与焦点验收；本轮没有进行 Telegram 真机验证。

[report]
task: WP-7-会话列表与远程踢下线
path: ok
gates: pass
escalation: -
