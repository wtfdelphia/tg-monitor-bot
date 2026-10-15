# PRE-001：远端 CI 与驱动门禁收口

## 当前证据

实施增量（2026-10-12）：用户在上轮明确列出本提案与 PRE-004 后再次要求实施，已
按此范围修复本地镜像来源、PR 引用查询和固定回归入口。下面保留原始故障证据；
当前结果见 `docs/plan/evidence/PRE-001-PRE-004-门禁收口-2026-10-12.md`。
仍缺新提交的远端 CI 与真实创建 PR 演练，不宣称本提案全部出口已满足。

核对基线：`96c6d4025054636a60f482e0e7cb8a69f8f31fa1`。

- `cloud/dev` 已指向核对基线；
- GitHub Actions 的 `static` job 已通过；
- `database` job 在 `docker compose up -d --wait` 阶段失败，日志为
  `minio Error unauthorized: access to the requested resource is not authorized`；
- 本地 `docker compose up -d --wait` 可以通过，但本地已有
  `quay.io/minio/minio` 镜像缓存；
- 本地 `docker manifest inspect` 对 `quay.io/minio/minio:latest` 返回
  `no such manifest`，对 `minio/minio` 返回访问被拒；
- 本机 `gh` 为 `2.4.0`，`gh pr view --json baseRefOid,headRefOid` 返回未知字段；
- `python3 scripts/ai-driver/check-regressions.py` 手工结果为 9 项通过、2 项按条件跳过。

## 需要批准的改动

### 1. 修复 CI 的对象存储镜像来源

`compose.yaml` 是本地与 CI 共用的部署工件，当前 MinIO 镜像引用在 GitHub runner
上无法拉取。需要在不改变本地服务契约、端口和 S3 兼容语义的前提下，选择一个可由
CI 无凭据拉取、可固定版本、并能通过现有健康检查的镜像来源；同时同步
`docs/design/eng/02-本地环境与迁移.md` 中的镜像说明，并在本地冷缓存条件下重跑。

该项涉及 `compose.yaml` 和部署文档。依据 `AGENTS.md` §五，部署工件属于保护路径，
本提案获批准前不直接修改。

### 2. 修复驱动的 GitHub CLI 兼容性

二选一：

- 将运行环境的 `gh` 升级到支持 `baseRefOid` 与 `headRefOid` 的版本，并把版本检查
  写进驱动启动前置；
- 改为当前版本可执行的 `gh api` 或 REST/GraphQL 查询，并保留 base 分支、base SHA、
  head SHA 三项严格比对。

无论选择哪条路径，都要用一次真实 PR 演练证明：创建成功、引用正确、查询失败不会被
误记为完成。该项涉及 `scripts/ai-driver/ai-run.sh`，属于保护路径。

### 3. 把回归检查纳入固定自动化入口

将 `scripts/ai-driver/check-regressions.py` 纳入固定执行入口，保留当前两条按条件跳过
的真实环境用例语义，不得把跳过改写成通过。应明确它运行在普通 CI、驱动发布检查，或
两处都运行，并记录失败时的出口状态。

该项涉及 `.github/workflows/ci.yml` 或 `scripts/ai-driver/`，属于保护路径。

## 出口判据

- CI 能在无预置私有镜像缓存的 runner 上完成 compose 启动；
- `static` 与 `database` job 均通过；
- 驱动能在实际 `gh` 版本下完成 PR base/head SHA 核验；
- 回归检查由固定入口重复执行，并保留失败即失败的退出语义；
- `docs/plan/编码实施前置待办.md` 中第 12、13、15 项可引用新的 CI 与驱动证据。

## 处理边界

本提案不代替 Telegram 真机、OIDC 或租户 `api_id` 申请；首发媒体范围已由
`docs/design/decisions/ADR-0031-首发包含媒体中转.md` 定案，相关实现与验收仍按
`docs/plan/编码实施前置待办.md` 的 P1/P2 条目推进。

[report]
task: PRE-001
path: blocked
gates: pass
escalation: docs/plan/proposals/PRE-001-远端CI与驱动门禁收口.md
