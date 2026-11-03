# PRE-010：GitHub Actions 流水线运行时告警收口

任务 id：`PRE-010-actions-runtime`。

本提案收口仓库全部活动 GitHub Actions workflow 的运行时告警，并将 CI 的 `cargo-deny`
从 `0.19.1` 升至 `0.20.2`。实现前复核 `dev` 上最新工作流与 Actions 版本，不以本地工作树
作为发布基线。

## 一、背景与目标

2026-11-03 的候选发布 run `41570869444` 已成功，但日志指出
`actions/upload-artifact@v4` 使用 Node.js 20，且 `ubuntu-latest` 正在迁移到 Ubuntu 26.04。
全仓检查还发现 Docker 发布 workflow 引用的四个 Docker Actions 声明 Node.js 20；CI、候选
二进制、正式二进制和 Docker 发布四条 workflow 均使用 `ubuntu-latest`。

Node.js 20 已从 GitHub Actions runner 移除，旧 Action 当前由 Node.js 24 兼容运行。Ubuntu
26.04 的 `ubuntu-latest` 切换期间，同一标签可能分配到不同镜像。

目标是将仓库所有活动 workflow 的 JavaScript Actions 迁移到 Node.js 24 runtime，并固定
Ubuntu runner 标签。`cargo-deny` 升至 `0.20.2`。不得改变 CI 卡口、Docker 镜像发布权限、
候选包来源校验、二进制构建目标、签名身份、provenance 内容或非稳定发布边界。

`cargo-deny 0.20.0` 清理了已弃用 CLI 选项并增加可选的 `bans.std-replacements` 检查；
本仓当前只调用 `cargo deny check`。升级后须用目标版本实跑现有配置。`0.20.2` 的补丁修复
针对 Windows 发布归档文件名；不改变本仓 Linux runner 的策略。

## 二、批准范围

### 1. 固定 runner

将 `.github/workflows/ci.yml`、`binary-candidate.yml`、`binary-release.yml` 和
`docker-publish.yml` 中的 `runs-on: ubuntu-latest` 固定为 `ubuntu-24.04`。更新 `ci.yml`
顶部 runner 说明，记录升级镜像前须验证 PostgreSQL client、Docker 与发布构建。

### 2. 升级 Node 20 Actions

只升级实际声明 `runs.using: node20` 的 Actions，并复核官方元数据与迁移说明：

- `binary-candidate.yml`：upload-artifact `v4` 升到 Node 24 版 `v6`；download-artifact
  `v4` 升到 Node 24 版 `v7`。
- `docker-publish.yml`：setup-buildx-action `v3` 升到 `v4`；login-action `v3` 升到 `v4`；
  metadata-action `v5` 升到 `v6`；build-push-action `v6` 升到 `v7`。

保持 inputs、outputs、artifact 名称和路径、镜像标签与推送目标不变。已经声明 Node 24 或
使用 composite runtime 的 Action 不作无关升级。

### 3. 升级 cargo-deny

在 `ci.yml` 将安装版本从 `0.19.1` 改为 `0.20.2`，保留 `--locked` 与
`cargo deny check`。使用当前 `deny.toml` 实跑；不得关闭检查、增加宽泛 allow/ignore 或
排除依赖图。此次不改 `deny.toml`、Cargo 依赖或 lint 策略，不启用新 lint。

### 4. 边界

- 不改变 workflow 权限、OIDC 身份约束、cosign 校验、触发条件或来源提交校验。
- 不改变候选 tag、候选版本、Pre-release 属性及 Release 不覆盖规则。
- Docker workflow 仍仅推送已配置的 GHCR 目标，不扩大触发分支与权限。
- 不增加 Secret，不访问生产环境，不触发正式二进制发布。

## 三、验收判据

1. 活动 workflows 的 JavaScript Actions 均声明 Node 24 或更新 runtime，运行日志不再报告 Node.js 20 deprecated。
2. 四条活动 workflow 的 runner 均显式使用 `ubuntu-24.04`，不再报告 `ubuntu-latest` 迁移告警。
3. `cargo-deny 0.20.2` 与当前配置检查通过，未新增忽略项或放宽策略。
4. CI 的 `static`、`database` job 全绿，PostgreSQL client 仍满足 R7 版本判据。
5. Docker 发布 job 的 Buildx、GHCR 登录、metadata 和 build-push 验证通过，镜像目标与触发范围不变。
6. `dev` 候选流程的来源校验、CentOS 7 构建、SBOM、签名与 Pre-release 创建通过。
7. 正式二进制发布流程不触发；没有生产凭据或生产服务访问副作用。

## 四、不在范围

- 不改变正式二进制发布 workflow 的触发条件、签名策略、权限或发布内容。
- 不升级未报告 Node.js 20 的 Actions，不做无关依赖更新。
- 不修改 `deny.toml`、Cargo 依赖版本或 cargo-deny 策略，不启用新 lint。
- 不改变 CentOS 7 目标、包内容、Sigstore/cosign 策略、provenance schema 或候选发布规则。
- 不创建正式稳定版，不把候选包提升为正式包。

## 五、交付顺序

1. 审核所有 workflow 的 Action runtime、runner、权限、触发器及发布副作用。
2. 更新 Node 20 Actions、runner 标签和 cargo-deny 版本，同步修正 CI runner 说明。
3. 静态核验 workflow，运行 cargo-deny 与完整 CI；验证 Docker 构建但不推送 GHCR；从 `dev`
   手动执行完整候选流程，正式发布只检查、不触发。
4. 检查日志告警、策略、镜像目标、签名、工件清单及 Release 元数据，清理本任务专属残留。

[report]
task: PRE-010-actions-runtime
path: escalated
gates: not-run
escalation: docs/plan/proposals/PRE-010-GitHub-Actions运行时告警收口.md
