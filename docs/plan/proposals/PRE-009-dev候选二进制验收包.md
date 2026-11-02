# PRE-009：dev 候选二进制验收包

任务 id：`PRE-009-dev-candidate-binary`。

本提案只增加候选验收发布路径，不发布正式版本，不修改现有正式二进制发布策略。
提案已批准，正在按 `docs/plan/backlog/PRE-009-dev-candidate-binary.md` 实施。候选发布
workflow 进入默认分支前不能手动触发；本提案不授权创建稳定版本或发布生产包。

## 一、背景与目标

当前 `.github/workflows/binary-release.yml` 只接受 `vMAJOR.MINOR.PATCH` 正式版本标签，
并要求该标签位于 `main` 历史中且与 Cargo workspace 版本一致。它会创建或更新普通
GitHub Release，不适合作为尚未批准稳定版时的目标机验收入口。

PRE-007 已确定原生包使用 CentOS 7 / glibc 2.17 构建、GitHub Release 分发及
Sigstore/cosign keyless 签名。目标机需要一个能核验签名和来源、又不会被误认为稳定版的
包；不应为验收提前创建正式 `vX.Y.Z` 标签。

目标是从 `dev` 的一个明确提交生成**候选包**：包内元数据、Release 标题和说明、标签及
签名来源均标示候选身份；Release 必须为 GitHub Pre-release；候选工作流与正式工作流隔离。

## 二、需要批准的改动

### 1. 独立候选工作流

新增 `.github/workflows/binary-candidate.yml`，不修改 `.github/workflows/ci.yml`、
`.github/workflows/binary-release.yml` 或 Docker 发布工作流。

- 只启用 `workflow_dispatch`，仅允许从 `refs/heads/dev` 运行；拒绝 PR、其他分支和
  不属于触发时 `dev` 历史的提交。
- 构建触发时的精确 `github.sha`，不接受调用者任意输入源码 SHA；日志和所有工件记录完整
  SHA。
- 发布前通过 GitHub API 确认该提交的 `static` 与 `database` 检查均完成且成功。若 CI
  记录不存在、未完成或失败，工作流失败关闭。
- 复用 PRE-007 的 CentOS 7 builder、glibc 2.17 检查、帮助命令和安装包预检；构建使用
  锁定工具链、`Cargo.lock` 与提交内 `.sqlx/`，不读取生产凭据或连接生产服务。
- 构建 job 仅读取源码与检查；签名 job 仅读取本次构建工件并取得 OIDC 身份；发布 job
  单独持有创建 tag/Release 所需的 `contents: write`。不授予部署、环境 Secret 或生产
  服务访问权限。

工作流文件位于 `.github/workflows/`，签名配置属于发布信任边界，因此本任务需经提案批准
后实施。

### 2. 候选身份与包格式

候选发布有独立标签和版本身份，不使用正式标签前缀 `v`。使用唯一的
`candidate-<run_id>-a<attempt>-<short_sha>` Release tag，以及
`MAJOR.MINOR.PATCH-candidate.<run_id>.<attempt>.<short_sha>` 候选 SemVer；run attempt
使同一 workflow run 的重跑也不会复用候选身份。

现有 `scripts/native-release-package.py` 只接受纯 `MAJOR.MINOR.PATCH`。需最小调整为同时
接受受限、经过校验的候选预发布版本，并将候选状态写入 `BUILDINFO.json`、`VERSION` 和
README。不能把任意字符串直接拼进路径或归档文件名。候选归档文件名必须包含 `candidate`
及唯一 run 标识，避免与正式包同名或覆盖。

候选包及 Release 清单至少明确包含：

- `channel: candidate`、`stable: false`、候选版本、完整源码提交和目标平台；
- 归档 SHA-256、构建器身份、构建工作流运行链接及 SBOM；
- “仅供非生产验收，不是稳定版，不提供稳定性或生产适用性声明”的醒目说明。

候选流程复用原生包的安装与校验能力，但安装工具必须保留候选标识，不能将候选版本报告为
稳定版本。目标机是否允许运行候选包由验收步骤显式确认，不得自动安装、启用服务或连接生产
数据库和外部服务。

### 3. 签名与来源证明

使用 GitHub Actions OIDC 和 cosign keyless 签名，不保存长期私钥。为候选工作流单独配置
可验证身份约束，issuer 固定为 GitHub Actions OIDC issuer，certificate identity 必须限定
到本仓库的 `binary-candidate.yml` 与 `refs/heads/dev`；不得复用或放宽正式发布工作流的
身份正则。

签署 `SHA256SUMS` 与 provenance。provenance 应记录候选 channel、候选标签、源码提交、
工作流路径与运行 ID、构建目标及归档 SHA-256。Release 附件包括候选归档、校验清单、
清单签名 bundle、provenance、provenance 签名 bundle、SBOM 和构建元数据。

目标机验收命令必须显式验证 issuer、certificate identity、签名 bundle、清单校验值、
provenance 中的提交和归档摘要。任何一项不匹配都拒绝验收；仅有 HTTPS 下载或 SHA-256
文件不足以证明发布身份。

### 4. GitHub Pre-release 与不可覆盖规则

- 使用不匹配 `v*.*.*` 的候选 tag；创建 GitHub Release 时必须设置 `--prerelease`。
- 同一候选 tag 或 Release 已存在时，工作流失败，不使用 `--clobber` 覆盖已有验收证据。
  重新构建使用新的 workflow run 和新的候选标识。
- Release 标题、正文、资产文件名和包内元数据都标明 `CANDIDATE / NOT STABLE`。
- 候选流程不创建 `vX.Y.Z` tag，不更新正式 Release，不触发正式二进制发布，也不推送
  GHCR 镜像。
- 候选验收通过后，正式包仍须从后续进入 `main` 的稳定版本提交重新构建并签名。不得通过
  重命名、复制 Release 附件或移动 tag 把候选包“提升”为正式包。

## 三、验收判据

1. 从 `dev` 对指定提交手动运行可产出候选包；从 PR、`main` 或其他分支运行会失败。
2. 源提交的 `static` 与 `database` 检查缺失、未完成或失败时，候选发布被拒绝。
3. 包内与 Release 元数据均显示候选版本、源码完整 SHA、目标平台及非稳定标识；归档名与
   Release tag 不会被正式版本误认。
4. 目标机使用公开验证材料能验证 cosign issuer、候选 workflow identity、清单和
   provenance；篡改归档、摘要、提交或身份时验证失败。
5. GitHub Release 显示为 Pre-release；不存在正式 `vX.Y.Z` tag、正式 Release 更新或
   GHCR 推送副作用。
6. CentOS 7 / glibc 2.17 的二进制、依赖、安装前置检查和 systemd unit 语法检查通过；
   目标机手动验收结果单独记录，不以构建机结果代替。
7. 候选流程不读取生产 Secret、不执行生产迁移、不安装到生产目录、不启动或启用服务，
   不连接生产数据库、Telegram、KMS、Redis 或 S3。
8. 同一候选 Release 已存在时流程拒绝覆盖；构建结束后清理本 workflow 创建的临时
   builder、容器和工作目录，不清理共享 Docker 资源。

## 四、明确不在范围

- 不创建或发布正式稳定版本，不定义 `main` 合并或稳定版批准规则。
- 不修改现有正式二进制发布 workflow 的触发条件、tag 规则、权限或正式 Release。
- 不修改 Docker 镜像发布流程，不将候选二进制发布到 GHCR。
- 不自动将候选包部署到任何机器；不授权生产安装、迁移、服务启用或真实数据操作。
- 不把候选验收通过等同 WP-8、生产就绪或上线批准；工作包出口仍按
  `docs/design/plan/00-里程碑与工期.md` 的定义单独裁定。

## 五、批准后交付顺序

1. 以最新远端 `dev` 为基线复核 PRE-007 与 PRE-008 工件、目标机验收边界和正式发布 workflow。
2. 确定候选 SemVer、Release tag、签名 identity 与 provenance 字段，并为包格式增加候选
   通道标识。
3. 新增独立候选 workflow，并拆分构建、OIDC 签名与 Release 写入权限；验证拒绝路径、签名
   来源与不可覆盖行为。
4. 在非生产 CentOS 7 目标机执行下载、签名验证、安装前置检查、systemd 验证及回滚检查，
   留存脱敏证据。
5. 候选验收结果通过后，再另行评估正式版本进入 `main` 与创建稳定 tag 的条件。

[report]
task: PRE-009-dev-candidate-binary
path: ok
gates: pending
escalation: docs/plan/proposals/PRE-009-dev候选二进制验收包.md
