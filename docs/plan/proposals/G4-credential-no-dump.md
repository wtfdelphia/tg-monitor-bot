# G4：凭据不可 dump 全出口收口提案

任务 id：`G4-credential-no-dump-audit`。

状态：已批准并完成代码实施（2026-11-07）。实际实施基线为 `cloud/dev` 合并提交
`880daccb4562bb743eccab9fdcf2cf541a94e333`；三类出口的隔离验证和本片定向测试已通过，
真实 Telegram、真实浏览器和生产环境仍未验收。PR CI 是本次变更的最终裁决。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G4；
`docs/design/eng/00-工程约定.md` §五；`docs/design/eng/01-配置与Secret.md` §4.5；
`docs/design/eng/03-测试策略.md` §三 R7；`docs/design/spec/05-安全与租户隔离.md` 的凭据边界。

## 一、现状与缺口

当前代码已有三道防线：

- `Secret<T>` 的 `Debug` 固定输出 `[REDACTED]`，并有嵌套结构反证；
- `identity_secrets` 的 Bot Token、Bot session 与 MTProto session 以信封加密写入，
  `api_hash` 不落库，身份审计日志只记录 `credential_set` 等布尔事实；
- 控制面内部错误统一返回 `INTERNAL` / `internal error`，QR 密码失败响应不回显密码；
  默认日志过滤器把 `sqlx` 限制为 `warn`。

这些证据分散在单元测试、数据库测试和 Rocky 运行时记录中，尚未按 G4 的“库内容、日志、
错误响应”三个出口统一裁决。审查还发现一个不能只靠补文档关闭的日志缺口：

1. `TelegramBotApiClient` 将 Bot Token 放在请求 URL 的 `/bot<token>/<method>` 路径中；
2. `reqwest` 传输错误已经映射为无底层详情的 `TelegramBotApiError::Request`，不会输出 URL；
3. 但 Telegram 或中间代理返回的 JSON `description` 会原样进入
   `TelegramBotApiError::Rejected.description` 或 `TelegramBotApiError::Api(String)`；
4. 控制 Bot 处理失败会用 `tracing::error!(error = %error, ...)` 记录该错误。

远端响应正文属于不可信输入。若代理、网关或测试服务把请求 URL 回显到 `description`，现有
`Display` 会把 URL 中的 Token 写入日志。业务只依赖 HTTP 状态与 `retry_after`，没有生产逻辑
依赖远端描述正文，因此不应保留这段不可信文本。

## 二、拟实施范围

### 2.1 Bot API 错误收敛

- `TelegramBotApiError::Rejected` 只保留 `status` 与 `retry_after`，不保存远端
  `description`；`Display` 和派生 `Debug` 均不得包含响应正文。
- Bot API 非 4xx 的失败只保留 HTTP 状态或固定内部分类，不把远端 `description` 放进
  `Api` 错误。代码内部生成的固定错误可继续使用固定字符串，但不得混入响应正文、请求 URL、
  Token 或响应 body。
- 不改变目标自检和租户通知的状态分类：429 仍读取 `retry_after`，其他 4xx 仍是确定拒绝，
  5xx/协议失败仍按可重试不可用处理。
- 不记录、不持久化 Telegram 原始错误正文。本片不新增表、迁移、角色、GRANT、RLS 或依赖。

### 2.2 三类出口反证

- **内存格式化与日志入口**：使用唯一合成 Token 和远端描述哨兵，覆盖 `Secret<T>`、
  `BotCredentials`、Bot API `Rejected`、非 4xx `Api` 以及控制 Bot 外层错误的
  `Debug`/`Display`；任何输出包含哨兵即失败。
- **数据库内容**：复用受限角色凭据测试，统一核对 Token、Bot session、MTProto session
  不以明文出现在凭据列和身份审计 JSON；`api_hash` 继续保持不落库。测试只使用合成值，
  不打印密文或明文内容。
- **HTTP 错误响应**：把合成敏感值放进内部错误源和 QR 密码请求，读取实际 Axum response
  body 与 header，确认只返回公开错误码和固定文案，不出现底层错误、密码、Token、session、
  `api_hash` 或数据库对象名。
- **日志配置**：保留 `DEFAULT_FILTER` 的行为反证，确认 `sqlx::query` INFO 被阻止且业务 INFO
  仍可输出；不把“默认过滤器正确”扩大成任意用户自定义 `RUST_LOG` 都安全。

## 三、验证与证据

实施后至少执行：

1. `cargo test -p tgm-core secret telemetry --lib`；
2. `cargo test -p tgm-app bot_api --lib` 及控制面错误响应定向测试；
3. `cargo test -p tgm-db --test credentials -- --nocapture`；
4. `cargo test -p tgm-app --test control qr_login_poll_and_password_reject_bot_source_and_unknown_login -- --nocapture`；
5. `scripts/ai-driver/ai-gates.sh --static-only`，以及在干净 Compose PostgreSQL 上运行数据库门禁；
6. 新增脱敏证据矩阵，逐项标记库内容、格式化/日志入口和 HTTP 错误响应的结果与边界。

反证必须至少覆盖：远端 `description` 直接等于 `/bot<synthetic-token>/getMe` 时，删除收敛逻辑
会使测试变红；去掉 `Secret<T>` 脱敏、把明文写入审计、或把内部错误公开文案改为底层错误时，
对应测试也必须失败。

## 四、边界与审批请求

本提案触及凭据与日志安全面，命中 `AGENTS.md` §五升级条件。获批前不修改
`TelegramBotApiError`、日志调用、HTTP 错误映射或凭据实现。

请批准后实施以下范围：

1. 丢弃 Bot API 远端错误正文，只保留状态、`retry_after` 和固定分类；
2. 补齐三类凭据出口的可执行反证与统一证据；
3. 保持 API 状态分类、重试行为、数据库结构和权限不变；
4. 即使本片全部通过，真实 Telegram 端到端、真实限流、生产副本和 WP-8 仍单独验收，
   不据此宣布 G3、G4 或 WP-8 通过。

```text
[report]
task: G4-credential-no-dump-audit
path: ok
gates: targeted-pass
escalation: ->
```
