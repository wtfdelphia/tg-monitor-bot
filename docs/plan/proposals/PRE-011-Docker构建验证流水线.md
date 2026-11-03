# PRE-011：Docker 镜像无推送构建验证

任务 id：`PRE-011-docker-build-validation`。

本提案只增加独立的 GitHub Actions Docker 构建验证入口，不更改 GHCR 发布 workflow，
不向 `main` 推送或开 PR。实现目标是先在以 `dev` 为 base 的 PR 中验证 GitHub-hosted
runner 上的镜像构建和非生产运行形态，再等待后续经批准的 `dev` 到 `main` 发布流程。

## 一、背景与目标

`.github/workflows/docker-publish.yml` 已存在于 `dev`，但当前 GitHub Actions workflow 列表
尚未登记该文件；它只在 `main` push 或手动触发，并以 `push: true` 发布 GHCR。仓库当前默认
分支是人工创建的 `main`，而 Docker 发布 workflow 尚未进入该分支。不能手动触发发布 workflow
来做构建试验，因为这会有发布副作用，且非 `main` 手动运行会被拒绝。

Docker-only WP-8 目标机验收已证明锁定镜像可完成隔离迁移、RLS 审计和合成租户清理
反证；它不能证明 Docker Actions 在 GitHub runner 上运行正常。PRE-010 的 Actions 版本升级
已进入 `dev`，其 Docker 发布 job 尚未实际运行。

目标是在 GitHub-hosted runner 上建立独立、无发布副作用的 Docker 验证：对目标分支为 `dev`
的 PR 构建仓库镜像，并运行既有 Docker-only 非生产验收脚本。该入口只验证构建和合成环境行为，
不签发发布身份、不访问 GHCR、不连接生产服务，也不改变正式镜像发布策略。

判据出处：`docs/design/eng/04-CI门禁.md` 的 CI 与发布边界；
`docs/plan/proposals/PRE-006-GHCR-main镜像发布.md` §二至§五；
`docs/plan/proposals/PRE-010-GitHub-Actions运行时告警收口.md` §三；
`docs/plan/evidence/WP-8-Docker-only非生产验收-2026-11-01.md`；
`docs/plan/evidence/WP-8-Docker目标机发布包预检-2026-11-01.md`。

## 二、批准后实施范围

### 1. 独立验证 workflow

新增 `.github/workflows/docker-build-validation.yml`，与 `ci.yml` 和
`docker-publish.yml` 分离。

- 仅响应目标分支为 `dev` 的 `pull_request`，覆盖 PR 打开、重新打开和新提交同步；不启用
  `push`、`workflow_dispatch`、`pull_request_target` 或定时触发。
- workflow 在 PR merge ref 上检出并验证该 PR 的合并结果，不接受任意输入 SHA。
- runner 固定为 `ubuntu-24.04`，符合 PRE-010 的 runner 约定。
- workflow 和 job 的 `GITHUB_TOKEN` 权限只读；不声明环境、不读取仓库 Secret，不执行登录，
  不授予 `packages: write`、`id-token: write` 或部署权限。
- 使用 PRE-010 已批准的 Docker Actions 版本与 `dev` 发布 workflow 对齐；Buildx 构建明确设置
  `push: false`，不生成 GHCR tag，不上传镜像到任何 registry。
- 不修改 `.github/workflows/ci.yml`、`.github/workflows/docker-publish.yml`，不改变现有
  workflow 的触发器、权限、标签、发布目标或 BuildKit 发布配置。

GitHub 对 `pull_request` 使用 PR merge ref 运行检查，但不能依赖新增 workflow 的同一个 PR
触发该 workflow。先将实现合入 `dev`，再由后续另一个以 `dev` 为 base 的 PR 触发并记录实际
run；若当时没有其他待合并 PR，使用仅补验收证据的后续 PR 触发。`workflow_dispatch` 要求
workflow 文件已在默认分支 `main`，本提案不将它作为入口，也不为此向 `main` 推送。

### 2. 构建后非生产检查

- 将构建镜像加载到 runner 本地 Docker daemon，不推送 registry。
- 验证镜像非 root 用户、运行时不包含源码和 Cargo 缓存、不含构建期 `SQLX_OFFLINE` 环境变量。
- 复用 `scripts/wp8-docker-acceptance.sh --image` 和仓库现有 Compose 工件，在随机命名的隔离
  project 中运行迁移、`audit-rls` 和合成租户清理反证；不复制或放宽脚本判据。
- 所有测试数据仅为合成数据；不得访问 PostgreSQL 生产端点、Telegram、KMS、Redis、对象存储或
  生产 Secret。
- 无论成功或失败，都只清理本 job 创建且可按随机 project/builder 名确认归属的容器、网络、卷、
  本地镜像和 builder；失败时保留足以诊断的日志，不执行全局 prune。

GitHub-hosted runner 的 Docker daemon、Compose 与隔离 PostgreSQL 属于 CI 测试环境。该检查不
替代目标机验收或生产备份恢复验收。

### 3. 明确的发布边界

- `.github/workflows/docker-publish.yml` 仍只在 `main` push 自动发布；手动运行仍按其既有规则拒绝
  非 `main` 分支。
- 本提案不把 GHCR 发布 workflow 合入 `main`，不执行真实发布，也不改变“从 `dev` 合入 `main`
  后触发镜像推送”的既定流程。
- 无推送构建通过，只表示 PR 对应源码可在 GitHub runner 上构建并通过合成 Docker-only 检查；不表示
  GHCR 凭据、registry 写权限、远端推送、provenance/SBOM 发布附着或生产部署已验收。
- 首次 GHCR 发布仍须在未来经维护人批准的 `dev` 到 `main` 合并后单独核验镜像引用、digest、
  provenance、SBOM、权限和回滚方式。

## 三、验收判据

1. workflow 合入 `dev` 后，由另一个以 `dev` 为 base 的 PR 实际触发；实现 PR 自身不作为唯一
   触发证据。
2. 构建步骤在 GitHub-hosted `ubuntu-24.04` runner 上完成；构建日志证明 `push: false`，没有
   registry login、GHCR tag 推送或 `packages: write` 权限。
3. 镜像运行用户和运行时内容检查通过；合成 PostgreSQL 环境中的迁移、`audit-rls` 与租户清理反证
   通过，结果与既有本地验收判据一致。
4. 成功和失败路径均只清理本 job 创建的 Docker 资源；失败日志可诊断，未执行全局 prune。
5. `ci.yml`、`docker-publish.yml` 的内容、触发范围、权限与发布目标保持不变；未向 `main` 推送，
   未创建 GHCR 镜像版本。
6. 证据明确标识 GitHub runner 无推送验证、目标机 Docker-only 验收、GHCR 首次发布和生产验收为
   不同判据。

## 四、不在范围

- 不修改或触发 `.github/workflows/docker-publish.yml`，不向 GHCR 或其他 registry 登录、推送。
- 不修改现有 CI 卡口定义、发布流程、分支保护、`main` 默认分支或 `main` 内容。
- 不构建多架构矩阵，不新增缓存策略、Secret、云账号权限或自动部署。
- 不改变 Dockerfile、数据库迁移、RLS、生产连接配置、租户删除语义或 WP-8 的出口判据。
- 不把 GitHub 合成环境结果当作 CentOS 7 二进制验收、生产环境验收或 WP-8 通过证明。

## 五、批准后交付顺序

1. 以最新远端 `dev` 为基线复核 PRE-006、PRE-010 与 Docker-only 脚本。
2. 新增独立只读、无推送 workflow，确认 PR merge ref、Docker 构建与资源清理边界。
3. 用 `actionlint` 和适用静态检查核验 workflow；合入 `dev` 后，通过后续以 `dev` 为 base 的 PR
   确认 GitHub job 实际触发并通过。
4. 归档脱敏 run 链接、构建 SHA、Docker-only 检查摘要和资源清理结果；保持 `main` 与 GHCR 未变。
5. PRE-011 验收完成后，再继续独立的 CentOS 7 候选二进制目标机验收；WP-8 的产品/法务输入、
   跨副本留存与生产运维验收仍按各自证据台账推进。
