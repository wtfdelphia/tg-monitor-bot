# PRE-010：GitHub Actions 运行时告警收口核验（2026-11-05）

任务 id：`PRE-010-actions-runtime`。

核对基线：`cloud/dev`，合并提交 `45c622c75df510b4bff1be0505aceb158e80ea59`。
本记录只核对已合入的 workflow 修改、远端运行结果和默认分支注册状态；不触发
GHCR 推送，不触发正式二进制发布，不访问生产服务。

判据出处：`docs/plan/proposals/PRE-010-GitHub-Actions运行时告警收口.md` §三；
`docs/design/eng/04-CI门禁.md`。

## 一、已核对通过

| 判据 | 结果 | 证据 |
|---|---|---|
| `dev` 上活动 workflow 固定 `ubuntu-24.04` | `pass` | `ci.yml`、`binary-candidate.yml`、`binary-release.yml`、`docker-build-validation.yml` 和 `docker-publish.yml` 均使用 `ubuntu-24.04` |
| Node.js 20 相关 Action 已升级 | `pass` | `checkout@v7`、候选包 `upload-artifact@v6` / `download-artifact@v7`、Docker Actions `v4`/`v6`/`v7` 已在 `cloud/dev` 核对 |
| `cargo-deny` 版本 | `pass` | `ci.yml` 执行 `cargo install cargo-deny --locked --version 0.20.2` |
| CI 远端裁决 | `pass` | push run `41959591439`，`cloud/dev` 最新提交，结果 `success` |
| 候选二进制流程 | `pass` | workflow_dispatch run `41664186480`，来源为 `dev` 提交 `4abc1152c9fc18b33b9ec0b0495ba02afd07f2d8`，结果 `success` |
| Docker 无推送构建 | `pass` | PR run `41956564138`，结果 `success`；该流程使用 Buildx 构建并执行 Docker-only 非生产验收，不推送 GHCR |

截至本次核验，已核对的运行记录没有出现 `Node.js 20` deprecated 或
`ubuntu-latest` 迁移告警。

## 二、尚未满足的判据

PRE-010 的 Docker 发布验收不能标记为通过，原因不是 Docker 构建失败，而是默认分支状态：

1. 仓库默认分支仍为 `main`。
2. `cloud/main` 当前提交为 `eaaaf0d4902d989ae74089ef424de0ba9c184d29`，其 workflow 目录只有
   `binary-candidate.yml`。
3. `docker-publish.yml`、`binary-release.yml`、`ci.yml` 和 `docker-build-validation.yml`
   尚未进入 `main`，因此 GitHub Actions API 当前只注册候选发布、CI 和 Docker build validation
   三条 workflow。
4. 因此 Docker GHCR 发布 job 没有实际运行；本次没有登录 GHCR，也没有推送镜像。

`cloud/dev` 上的 `docker-publish.yml` 仍保持原批准语义：只响应 `main`，需要 `packages: write`，
启用 BuildKit provenance/SBOM，并在 workflow_dispatch 时拒绝非 `main`。但这些代码存在于
`dev` 不等于已具备生产发布能力。

## 三、裁决

| 范围 | 状态 | 结论 |
|---|---|---|
| PRE-010 代码实现 | `pass` | runner、Action runtime 和 `cargo-deny 0.20.2` 已落地 |
| CI 与候选流程 | `pass` | 已有远端成功运行记录 |
| Docker 无推送验证 | `pass` | 构建与非生产验收通过 |
| Docker GHCR 发布验收 | `blocked-input` | 需要默认分支注册和稳定发布边界方案 |
| PRE-010 任务整体 | `active` | 不提前改 backlog 为 `done` |

默认分支发布注册涉及 `main`、GHCR 写权限、稳定镜像标签和正式二进制发布边界，已另立
`docs/plan/proposals/PRE-013-默认分支稳定发布注册.md`，批准前不执行。

```text
[report]
task: PRE-010-actions-runtime
path: escalated
gates: targeted-pass
escalation: docs/plan/proposals/PRE-013-默认分支稳定发布注册.md
```
