# G2：HTTP 请求关联标识与 tracing 上下文

任务 id：`G2-http-request-tracing-context`。

状态：实现与定向验证完成；完整本地门禁和远端 CI 待执行，最终以 `dev` PR CI 裁决。

## 一、现状与缺口

G2 出口要求 SDD 固定 tracing span 字段；`docs/design/eng/00-工程约定.md` §三要求内部
数据库错误映射为不透明的 500 并附 `request_id`，§五要求 span 带 `tenant_id` 与
`request_id`，敏感参数不得进入 span。

当前控制面在 `crates/app/src/control.rs` 中统一实现 `ApiError::IntoResponse`，内部错误写入
日志，响应包含错误码 header；但 `ErrorResponse` 没有请求关联标识，错误日志也没有记录
`request_id`。代码中未找到控制面 HTTP 请求级 tracing span 或 `request_id` 实现。可信租户
上下文由认证后得到的 `AuthContext` 提供，不应从请求路径、查询参数或调用者提供的 header
推导。

本提案只补齐该运行时关联能力，不据此宣布 G2、G3 或 G4 通过。G0/G1 的冻结与外部证据仍
按既有记录处理。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G2；
`docs/design/eng/00-工程约定.md` §三、§五；
`docs/design/eng/03-测试策略.md` §三；`docs/design/eng/04-CI门禁.md`。

## 二、拟议行为

1. 每个控制面 HTTP 请求由服务端生成不可预测的关联标识。请求方提交的同名 header 不得被
   原样信任或用于覆盖服务端标识。
2. 在响应中通过单一、文档化的 header 返回该标识；成功与失败响应使用同一规则。错误 JSON
   包络与现有错误码、状态码保持不变，避免为关联日志而改写客户端错误契约。
3. 请求 span 记录 `request_id` 和低敏感度的请求信息；只在认证后从可信 `AuthContext` 记录
   `tenant_id`。未认证请求不推测、不回显租户标识。
4. 内部错误日志记录与响应相同的 `request_id`，仍只返回不透明错误。不得记录 SQL 参数、凭据、
   session、原始请求体、query string 或未经模板化的资源路径。
5. 对 span 字段和 header 命名进行一次性定稿并同步工程文档与 OpenAPI；不增加数据库字段或
   迁移，不改变认证、RLS、权限、错误码语义及生产日志级别。

## 三、验收判据

1. 未提供请求标识、提供伪造标识及重复标识时，服务端均生成独立关联标识；响应 header 与
   服务端错误日志中的标识一致。
2. 一般成功响应、认证失败响应和内部错误响应均按定稿规则返回关联标识；现有状态码、错误码、
   错误包络和跨租户 404 行为不变。
3. 认证请求的 span 租户字段只来自已验证的 `AuthContext`；未认证请求及伪造租户输入不能
   产生租户字段。
4. 注入唯一哨兵的内部错误、敏感请求参数和 query string 均不出现在响应或采集日志中；仅
   `request_id` 用于关联。
5. 覆盖 handler 错误、认证中间件提前返回及正常响应的集成测试通过；OpenAPI 与文档检查通过，
   不修改 `eng/04` 门禁定义。
6. 代码改动通过定向测试及完整 `scripts/ai-driver/ai-gates.sh`；最终以 `dev` PR CI 裁决。
   不连接生产服务、不使用真实凭据、不触发 Telegram 真机流程。

## 四、范围与停止条件

- 本片限于控制面 HTTP 请求标识、请求 span、内部错误日志关联、测试和对应文档。
- 不将 `tenant_id` 加入 API body、URL、查询参数或错误详情。
- 不信任反向代理或客户端传入的请求 ID；若需代理透传，另行评估可信代理边界。
- 不扩展到 Bot API、worker、数据库连接日志或第三方收集器；这些日志路径需单独验证。
- 若实现需要改变错误 JSON、日志保留/访问策略、认证中间件顺序或引入新依赖，先停止并补充
  设计，不在本片隐式扩大范围。
- 本片通过只补齐 G2 的工程实现证据，不自动冻结 SDD/TS，也不宣布 G2、G3、G4 或 WP-8
  通过。

## 五、批准后执行顺序

1. 核实控制面 router/middleware 顺序与认证上下文注入点，定稿关联 header 和 span 字段。
2. 以最小改动实现服务端请求标识、认证后租户 span 记录与内部错误日志关联。
3. 增加响应一致性、伪造输入、未认证、跨租户和脱敏反证测试。
4. 同步 OpenAPI/工程约定与 G2 当前证据，运行完整门禁并经 `dev` PR CI 裁决。

## 六、本次实施记录

- 控制面最外层中间件生成 128 位系统随机 request ID，以 URL-safe Base64 返回
  `X-Tgm-Request-Id`；忽略调用者提供的同名 header，包括重复值。随机源失败时返回 503，
  不回退到可预测标识。
- 请求 span 只记录 request ID、HTTP method 和响应状态，不记录路径、查询串或请求体。
  租户字段只由认证后的 `AuthContext` 写入。
- 内部错误响应维持原错误码、状态码和 JSON 包络；日志记录错误类别、错误码及相同
  request ID，不再格式化底层错误载荷。
- 已覆盖 OpenAPI 响应头、缺省/伪造/重复 request ID、内部错误与 query 哨兵脱敏、认证租户
  span、未认证及 CSRF 提前拒绝的租户输入反证。定向结果：控制面单元测试 25 项通过，认证
  集成测试 3 项通过。
- OpenAPI 导出已更新；完整 `ai-gates.sh` 与远端 CI 尚未完成，G2/G3/G4/WP-8 不据此宣布通过。
- `ai-gates.sh --static-only` 的 `fmt` 与 `clippy` 通过；`offline-build` 在首次完整编译期间
  因保护磁盘余量而中止。之后清理本任务专用 target，释放约 9 GB；完整门禁待 PR CI 裁决。

```text
[report]
task: G2-http-request-tracing-context
path: ok
gates: targeted-pass; static-fmt-clippy-pass; offline-build-interrupted-for-disk-safety; CI-not-run
escalation: ->
```
