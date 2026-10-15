# PRE-004：WP-5 OpenAPI 门禁收口

## 当前状态

发布增量（2026-10-12）：前置基础、后端、控制台已分别创建 PR #3/#4/#5，base 均为
`dev`，远端 CI 均全绿。当前 PRE-004 发布工作树已分别对双契约制造漂移并证明固定
static 入口拒绝，产物恢复逐字节一致；完整卡口和本批 CI 分别验收，结果见
`docs/plan/evidence/PRE-004-独立发布验收-2026-10-12.md`。
以下“未提交”保留为授权前状态，不代表前置批次当前状态。

实施增量（2026-10-12）：用户在上轮明确列出本提案与 PRE-001 后再次要求实施。
双契约检查已接入本地和 CI static 卡口；真实产物漂移拒绝及卡口失败传播已验证。
以下“仍跳过”是实施前状态，当前结果见
`docs/plan/evidence/PRE-001-PRE-004-门禁收口-2026-10-12.md`。仍未提交或创建 PR，
不能把本地门禁通过记为远端 CI、干净提交树验收或整个 WP-5 完成。

控制面已经由 `utoipa` 生成租户契约与平台管理契约，仓库内的
`openapi.json`、`openapi-admin.json` 均已生成。当前本地执行：

```bash
SQLX_OFFLINE=true cargo run --quiet --bin tgm -- openapi --check
```

结果为契约一致。

任务代码卡口与可信数据库卡口也已通过；但 `scripts/ai-driver/ai-gates.sh`
仍按旧基线跳过 `openapi-check`，`.github/workflows/ci.yml` 仍以注释保留该步骤。
因此 WP-5 的“OpenAPI 契约门禁通过”目前只有手工可执行证据，还没有进入机器裁决链。

## 需要批准的改动

### 1. 任务驱动门禁

- 在 `scripts/ai-driver/ai-gates.sh` 中将 `openapi-check` 纳入 static 卡口顺序；
- 保持该步骤不连接数据库，使用与 static job 相同的构建目标目录；
- 失败时保留 `tgm openapi --check` 的非零退出码与文件差异信息。

### 2. CI 门禁

- 在 `.github/workflows/ci.yml` 的 static job 中加入
  `cargo run --quiet --bin tgm -- openapi --check`；
- 删除当前“WP-5 前跳过”的注释；
- 保持 workflow 中其余卡口顺序与环境边界不变。

### 3. 定义处同步

- 更新 `docs/design/eng/04-CI门禁.md` 的 C3 状态说明；
- 更新 `docs/design/plan/00-里程碑与工期.md` 的 WP-5 出口记录；
- 不手工编辑生成的 JSON，继续以代码生成结果为事实源。

## 验收判据

- 干净工作树上 `tgm openapi --check` 通过；
- 人为修改任一契约文件后，static 卡口以非零退出；
- 还原契约文件后，static 与 database 两个 CI job 均恢复通过；
- `openapi.json` 与 `openapi-admin.json` 不包含租户管理路由泄漏到 admin 契约的情况；
- 不修改迁移、RLS、GRANT、审计 SQL、Telegram 真机流程或 OIDC 外部验证。

## 升级原因

本任务需要修改 `scripts/ai-driver/ai-gates.sh`、`.github/workflows/ci.yml` 以及
`docs/design/eng/04-CI门禁.md`。前两者属于门禁与发布保护路径，后者是卡口唯一定义处，
命中仓库根 `AGENTS.md` §五 的安全/门禁升级条件，不能由执行器直接改动。

上述同步已按用户后续实施要求在本地落地；原来的“批准前不修改”边界没有扩展到
真实凭据、提交或远端发布。仍须在 CI 中完成当前提交的真实裁决。

[report]
task: PRE-004-WP5-OpenAPI门禁收口
path: blocked
gates: pass
escalation: docs/plan/proposals/PRE-004-WP5-OpenAPI门禁收口.md
