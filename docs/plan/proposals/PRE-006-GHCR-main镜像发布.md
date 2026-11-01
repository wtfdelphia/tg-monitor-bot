# PRE-006：合并到 `main` 后自动推送 GHCR 镜像

任务 id：`PRE-006-ghcr-main-publish`。

状态：已批准，待首次 `main` 发布验收。基线为 `cloud/dev` 当前提交
`da19a62ad6652d33f8940f02df6f5f18fdc9c642`。本提案只申请新增独立的 GitHub Actions
Docker 发布 workflow，不修改现有 `.github/workflows/ci.yml`，不自动部署目标机器或生产环境。

## 一、背景与目标

当前仓库已有 `Dockerfile` 和 WP-8 Docker-only 非生产验收脚本，但现有 `ci.yml` 只执行
Rust、数据库、审计和契约门禁，没有自动构建或推送应用镜像。需求是将 Docker 镜像构建流程
与现有门禁 workflow 分开，并在代码合并到 `main` 后自动推送镜像。

目标是新增一个独立 workflow，触发条件仅为 `push` 到 `main`，使用 GitHub Container
Registry（GHCR）发布镜像。镜像内容必须来自触发该次 `main` push 的提交，标签和摘要必须
可追溯；workflow 不负责生产部署，不启用生产调度，不写入或读取生产凭据。

判据出处：`docs/design/eng/04-CI门禁.md` 的 CI 分层约定；
`docs/design/ops/00-部署与运行时.md` 的镜像非 root、配置外置和发布边界；
`docs/plan/proposals/WP-8-docker-only-nonproduction-acceptance.md` 的 Docker-only 验收边界。

## 二、批准后实现范围

1. 新增独立 `.github/workflows/docker-publish.yml`，不修改现有 `ci.yml`。
2. 触发器只允许：
   - `push` 到 `main`；
   - 手工 `workflow_dispatch`，但 workflow 拒绝非 `main` 分支的手工运行，避免从任意分支或任意提交推送镜像。
3. 使用 `actions/checkout`、Docker Buildx、GHCR 登录和 Docker metadata/action；所有 action 使用明确的大版本或经仓库批准的固定版本，不使用未说明的任意浮动脚本。
4. workflow 权限最小化：仅授予 `contents: read` 和 `packages: write`；不授予 `id-token: write`、云厂商权限或仓库管理权限。
5. 推送镜像到当前仓库对应的 GHCR 地址：`ghcr.io/${{ github.repository }}`，镜像名统一小写化，避免仓库名称大小写导致 registry 拒绝。
6. 标签至少包括：
   - `main`：当前主线滚动标签；
   - `sha-<短提交>`：不可变提交标签；
   - `v<版本>` 只有在仓库已有正式版本标签策略并明确批准后才加入，本提案默认不创建版本标签。
7. 使用 Buildx 构建并推送，并启用 `provenance: mode=max` 与 `sbom: true`；构建上下文遵守 `.dockerignore`，不向构建参数或标签写入秘密。
8. 构建上下文遵守仓库 `.dockerignore`，镜像运行阶段保持非 root，不把 `.env`、Telegram session、二维码、Cargo 缓存或源码复制到最终镜像。
9. workflow 运行日志输出镜像引用、提交 SHA 和 digest；不输出环境文件、连接串、token、`api_hash`、KMS 值或 session 内容。
10. 可选地在推送前执行不连接外部生产服务的镜像形态检查，例如验证运行用户为 `10001:10001`、最终镜像不含 `/src` 和 Cargo 缓存；不得把 Docker-only 合成验收伪装为生产验收。

## 三、明确不在本提案范围

- 不修改现有 `.github/workflows/ci.yml`，不把 Docker 发布 job 合并进现有代码/数据库门禁。
- 不在 PR、`dev` push 或普通分支 push 时推送镜像；如后续需要预览镜像，另提方案并使用隔离标签。
- 不自动部署 OCI VPS、Docker Compose 生产栈、systemd timer 或任何目标机器。
- 不写入 GHCR 以外的 ECR、Docker Hub 或云账号，不增加 AWS/OIDC 权限。
- 不把 GHCR 镜像推送成功视为 WP-8、G5 或生产验收通过。
- 不改变数据库表、迁移、RLS、GRANT、生产配置或租户删除语义。
- 不将 GHCR `main` 标签当作生产批准；生产使用仍需人工确认提交、digest、配置、迁移和回滚计划。

## 四、发布与失败语义

- Docker 构建失败：workflow 失败，不推送任何镜像。
- GHCR 登录或推送失败：workflow 失败；不执行目标机部署。
- 同一提交重复运行：允许覆盖 `main` 标签，但 `sha-<短提交>` 标签必须指向同一来源提交；实现需记录 digest，禁止用 `latest` 作为唯一发布标识。
- `main` 上的提交未通过现有 CI：本 workflow 不自动读取其他 workflow 状态，是否允许合并由仓库分支保护规则决定。应在实现 PR 中检查并记录：自动发布发生在合并之后，分支保护必须要求现有 `ci.yml` 通过后才能合并。
- 镜像 digest、提交 SHA 和构建日志是发布证据；不把本地 Docker-only 预检结果直接写成 GHCR 生产验收。

## 五、验收判据

1. `ci.yml` 内容不变，新增 workflow 能被 GitHub Actions 识别且独立运行。
2. PR 分支、`dev` push 不触发推送；合并提交进入 `main` 后触发一次发布。
3. workflow 使用最小权限，成功构建并推送 GHCR 镜像。
4. GHCR 中至少存在 `main` 与 `sha-<短提交>` 两个可追溯标签，并能查询同一 digest。
5. 日志包含来源提交和 digest，不包含生产密钥、Telegram token、`api_hash`、KMS Secret、session 或二维码。
6. 失败路径不执行生产部署、不启用调度、不修改数据库。
7. 实现提交前运行 YAML 语法/静态检查；如仓库已有 Docker-only 验收入口，则使用当前已加载镜像或构建产物进行适用检查，并清理专属 builder、临时容器和构建缓存。
8. 证据明确区分：镜像发布成功、Docker-only 非生产验收、生产部署/生产验收三者不是同一判据。

## 六、已确认输入与后续前置条件

- 已批准 GHCR 作为唯一镜像 registry。
- 已批准启用 `workflow_dispatch` 推送；实现拒绝非 `main` 分支的手工运行，因此不会从任意分支发布镜像。
- 已批准启用 BuildKit `provenance: mode=max` 与 `sbom: true`。
- `main` 分支保护目前尚未配置；首次稳定版从 `dev` 合入 `main` 前必须配置并验证要求现有 `static` 和 `database` 检查通过，避免“合并即自动发布”绕过代码门禁。该配置不在本提交范围内。
- 程序二进制包构建属于独立发布工件和供应链路径，涉及目标平台、归档格式、签名/校验、版本与回滚策略，另立 `PRE-007` 提案，不与本 GHCR workflow 混合。

## 七、批准后交付

本提案已批准。实现只包含本提案范围内的独立 workflow，并在一个提交中交付。实现完成后推送到
`dev` 开 PR；等待 CI 通过并合入 `dev`。只有该实现随后合入 `main`，才会触发真实 GHCR
推送。首次真实推送前，应由维护人核对 GHCR 包可见性、仓库权限、分支保护和回滚 digest。
