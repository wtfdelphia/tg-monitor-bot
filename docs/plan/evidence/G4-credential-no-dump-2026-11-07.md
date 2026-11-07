# G4 凭据不可 dump 三出口收口证据（2026-11-07）

任务：`G4-credential-no-dump-audit`。

基线：`cloud/dev@880daccb4562bb743eccab9fdcf2cf541a94e333`，PR #95 合入提交。
实现分支：`codex/G4-credential-no-dump-audit`。本证据只记录当前分支的本地隔离验证，
不是合并后的 CI 结果。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G4；
`docs/design/eng/00-工程约定.md` §五；`docs/design/eng/01-配置与Secret.md` §4.5；
`docs/design/eng/03-测试策略.md` §三 R7；`docs/design/eng/04-CI门禁.md`；
提案 `docs/plan/proposals/G4-credential-no-dump.md`。

## 一、修复范围

Bot Token 用于 `/bot<token>/<method>` 请求路径。此前 Telegram 或中间代理返回的
`description` 会进入 `TelegramBotApiError` 的 `Display`，再经控制 Bot 外层错误进入日志。
远端正文若回显请求路径，就会泄漏 Token。

本次移除 `Rejected` 保存的远端描述，并以 `Missing`、`Server` 取代保存远端文本的 `Api`。
错误只保留 HTTP 状态、固定分类及 `retry_after`，不保存或格式化远端响应正文。没有新增
迁移、表、角色、GRANT、RLS 或依赖，也没有改变 `Secret<T>` 和凭据加密的实现。

目标自检与租户通知的分类保持原语义：429 使用 `retry_after`，其他 4xx 是确定拒绝，
5xx 和缺少必要响应字段仍为可重试不可用。将目标自检结果映射抽成纯函数后，以定向测试
固定了这一行为。

## 二、三出口验证

| 出口 | 验证 | 结果与边界 |
|---|---|---|
| 数据库内容 | `crates/db/tests/credentials.rs` | `pass`：14 passed、0 failed。以合成值检查 Bot Token、Bot session、MTProto session 的密文列；`api_hash_ciphertext` 保持为空，身份审计 JSON 不含 session。只检查指定列与审计出口，不声称扫描任意数据库物理页、WAL 或备份 |
| 内存格式化与日志入口 | `tgm-core`、`bot_console.rs` 与既有脱敏测试 | `pass`：`tgm-core` 80 passed、0 failed；Bot API 定向测试 3 passed、0 failed。`Secret<T>`、`BotCredentials` 的 Debug 不含合成明文；403/503 的 `description` 直接回显 `/bot<synthetic-token>/getMe` 时，直接错误和控制 Bot 包装错误的 `Debug`/`Display` 均不含 Token 或描述 |
| HTTP 错误响应 | `control.rs` 内部错误响应与 QR 轮询/密码测试 | `pass`：实际 Axum 500 响应只含 `INTERNAL` 和固定文案，body 不含合成数据库错误哨兵或 `identity_secrets`；公开错误码 header 保持不变。QR 密码请求用合成密码，轮询与密码响应均不回显该值，并保持 `cache-control: no-store` |
| 默认 SQL 日志过滤 | `crates/core/src/telemetry.rs` 与配置测试 | `pass`：真实 subscriber 证明默认过滤器阻止 `sqlx::query` INFO，同时业务 INFO 可通过；无 `sqlx=warn` 指令的反例保持可检测。生产显式 `RUST_LOG` 的启动校验另行保留，不把默认过滤器测试扩大为任意自定义配置都安全 |

Bot API 反证不是仅检查错误类型：测试把唯一合成 Token 嵌入远端描述，再格式化实际错误
及外层错误。若恢复保存并显示远端 `description`，该用例会失败。数据库断言检查密文字节和
审计 JSON，而不是只验证读取接口能脱敏；HTTP 用例读取实际 response body/header，不是只
检查内部错误枚举。

未执行真实 Telegram 服务回显、宿主机 journal 全量扫描或任意外部日志收集器扫描。本片证明
已列出的应用入口，不证明操作系统交换区、崩溃转储、数据库备份或第三方代理绝不保存凭据。

## 三、门禁与异常处置

| 项目 | 结果 | 记录 |
|---|---|---|
| 静态门禁 | `pass` | `scripts/ai-driver/ai-gates.sh --static-only` 全部通过 |
| 在线 SQLx 一致性 | `pass` | 对已迁移的独立 schema 库执行 `cargo sqlx prepare --workspace --check`；通过且无 `unused queries found` |
| 迁移与重入 | `pass` | 独立数据库应用 57 个迁移；强制重放 57 个迁移后库状态逐字节一致 |
| RLS 审计与反证 | `pass` | 28 条断言、0 条报红；双遍反证全部通过，最终快照与第一遍一致 |
| workspace 测试 | `pass` | `cargo test --workspace` 退出 0，实际执行测试均通过 |
| 格式与空白 | `pass` | `cargo fmt --all --check`、`git diff --check` 通过 |

完整 `ai-gates.sh` 没有作为单次命令全绿，不能把上表写成完整脚本通过。数据库阶段先遇到
空 schema 库的在线 SQLx 检查及 Docker/I/O 问题，随后按相同命令逐项重跑并通过。最终仍由
PR CI 统一裁决。

首次手工临时库继承共享 `template1` 的触发器与 PUBLIC CONNECT，导致 C5/C7/C16/C17 报红。
该结果未计为隔离验证通过；随后使用 `template0` 和仓库要求的 ACL 重建，审计与双遍反证
才通过。没有修改共享角色、历史迁移或共享数据库来规避失败。

## 四、裁决与边界

本片三出口的隔离验证通过，关账矩阵可以把“凭据不可 dump”这一项从尚未汇总更新为有
隔离证据；这不是 G4 整体通过。kill 后不丢不重、真实 Telegram 限流与端到端、生产副本和
WP-8 外部验收仍未完成，G3、G4、WP-8 均保持未通过。

测试全部使用合成凭据，不连接生产 PostgreSQL、Telegram、KMS、Redis、对象存储或日志服务。
Telegram 真机、真实浏览器和外部凭据测试仍为 ignored，不计入通过项。

验证结束后，已定向删除本任务的两个临时数据库、停止的 Compose 容器和网络，以及临时目录；
保留共享 `tgm-pre008-acceptance-*` 栈。确认编译与测试进程结束后，以 `cargo clean` 清理本
worktree 专属构建产物 27.3 GiB；未删除源码、`.sqlx`、测试证据或其他 worktree 的产物。

```text
[report]
task: G4-credential-no-dump-audit
path: ok
gates: targeted-pass
escalation: ->
```
