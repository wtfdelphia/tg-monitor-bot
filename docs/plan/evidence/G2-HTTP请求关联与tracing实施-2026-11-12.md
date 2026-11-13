# G2：HTTP 请求关联与 tracing 实施（2026-11-12）

任务：`G2-http-request-tracing-context`。

基线：`cloud/dev@01e8a7cfe26ed820c04fe6d27860bd0bc3d39de3`；实施依据为已批准的
`docs/plan/proposals/G2-HTTP请求关联与tracing上下文.md`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G2；
`docs/design/eng/00-工程约定.md` §三、§五；
`docs/design/eng/03-测试策略.md` §三；`docs/design/eng/04-CI门禁.md`。

## 一、实现

- 控制面最外层中间件为每个请求生成 128 位系统随机数，并以 URL-safe Base64 返回
  `X-Tgm-Request-Id`。客户端同名 header（包括重复值）不参与生成或覆盖。随机源失败时返回
  503，不回退到可预测标识。
- `http.request` span 记录 `request_id`、HTTP method 和响应状态；不记录原始路径、查询串或请求体。
- 租户 `tenant_id` 只在认证成功后从 `AuthContext` 写入 span。未认证请求以及路径、query、header
  中自行提交的租户值不进入 span。
- 内部错误继续保持既有状态码、错误码和 JSON 包络。日志记录错误类别、错误码及同一
  `request_id`，不格式化底层错误载荷。
- public OpenAPI 每个已描述操作的响应都声明 `X-Tgm-Request-Id`；admin OpenAPI 无独立操作路径，
  仅保留其既有 schema 定义。未知路径 404 由 router 外层中间件覆盖并返回 request ID。

## 二、验证

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check` | pass |
| `git diff --check` | pass |
| `cargo clippy -p tgm-app --all-targets -- -D warnings` | pass |
| 控制面单元测试 `control::tests` | pass，25 项 |
| `auth_me_` 与缺失 CSRF SQLx 集成测试 | pass，3 项；验证可信租户 span、伪造租户输入、未认证请求与认证中间件提前拒绝 |
| 请求 ID（含缺省、伪造及重复 header）、未知路径 404、内部错误响应/日志与 query 哨兵脱敏测试 | pass |
| `tgm openapi --check` | pass |
| `scripts/ai-driver/ai-gates.sh --static-only` | `fmt`、`clippy` pass；`offline-build` 首次编译期间因保护磁盘余量而中止，后续卡口 not-run |
| 远端 CI | not-run；待 `dev` PR 裁决 |

数据库集成测试使用现有本地非生产测试 PostgreSQL；没有删除、重建或清理共享 Compose 数据卷。

## 三、状态与边界

实现按已批准提案完成，定向验证通过；完整门禁和远端 CI 尚未执行，因此不据此宣布 G2、G3、
G4 或 WP-8 通过。未修改迁移、RLS、GRANT、`eng/04` 门禁、生产服务或 GitHub 仓库设置；未触发
Telegram 真机流程、GHCR 推送或正式 Release。

定向验证和静态门禁复用 `/tmp/tgm-g4-acceptance-fix-20261110-target`。首次完整离线编译时根分区
余量降至约 7.4 GB，故中止后续门禁；随后经 `cargo clean --target-dir` 清理专用 target，释放约
9 GB。结束时根分区可用约 17 GB。

```text
[report]
task: G2-http-request-tracing-context
path: ok
gates: targeted-pass; static-fmt-clippy-pass; offline-build-interrupted-for-disk-safety; CI-not-run
escalation: ->
```
