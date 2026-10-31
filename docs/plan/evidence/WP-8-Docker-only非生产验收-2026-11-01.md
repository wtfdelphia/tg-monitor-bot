# WP-8：Docker-only 非生产验收记录（2026-11-01）

任务：`WP-8-docker-only-nonproduction-acceptance`。

基线：`cloud/dev` 合并提交 `960ee67d698ac8ad0915ac6b73e270618d7a5473`；提案批准提交为
`a9955f8b5ad4906669a0319e93694d1ecb32a282`。本记录只证明隔离 Docker 环境中的镜像、迁移、
RLS 审计和合成租户清理链路，不证明生产安装、生产备份恢复、生产告警或 WP-8/G5 已通过。

本轮没有连接生产数据库、Telegram、KMS、Redis、对象存储或真实备份；没有使用真实租户、
真实 token、session、`api_hash` 或生产 Secret。没有启用 `tgm-tenant-purge.timer`，没有删除
任何真实数据。

## 一、运行工件

| 项目 | 结果 | 记录 |
|---|---|---|
| Docker 构建 | `pass` | 多阶段构建，`cargo build --release --locked -p tgm` 成功 |
| 镜像 ID | `pass` | `sha256:71b0ad32a2665ebc8bbd70b9049cde013468509043ec621ffa6309a37ccde1a6` |
| 镜像归档 SHA-256 | `pass` | `7fe471410584cfe1e51f72cea96bf1052040f3e490eefb2a2a101e0681ed080f` |
| 运行用户 | `pass` | `10001:10001`，不是 root |
| 运行时内容 | `pass` | 无 `/src`、`/root/.cargo` 和构建期 `SQLX_OFFLINE` 环境变量 |
| Compose 隔离 | `pass` | 随机 project、独立 internal network 和具名 PostgreSQL volume |
| 迁移 | `pass` | `tgm migrate` 成功应用 56 个版本 |

## 二、审计与清理探针

| 场景 | 结果 | 证据摘要 |
|---|---|---|
| RLS 审计 | `pass` | 28 条断言、0 条报红；仍遵守“静态审计不等于策略正确”的限制 |
| 未到期租户 | `pass` | `29 days` 的 `deleted` 租户返回 `not_due`，租户保留 |
| 非 deleted 租户 | `pass` | `active` 租户返回 `not_deleted`，租户保留 |
| advisory lock | `pass` | 锁持有期间返回 `locked`；释放后重跑成功删除 |
| 幂等重跑 | `pass` | 已删除租户再次检查返回非成功状态，不重复删除 |
| 共享事件引用 | `pass` | 跨租户引用期间返回 `blocked`；解除引用后重跑成功 |
| 非目标租户保护 | `pass` | 目标租户删除后，非目标 `active` 租户仍保留 |

清理命令最终输出：`candidates=1 completed=1 failed=0 batches=7 rows_deleted=1 blocked=0 stopped=false`。

## 三、资源清理

验收脚本只删除本次随机命名的 Compose project、PostgreSQL volume、internal network、镜像和
Buildx builder。最终 project 为 `tgm-wp8-acceptance-1001-2483144`；脚本输出确认该 project
已移除。未执行 `docker system prune --volumes`，未触碰其他 Compose 栈。

验收后主机检查：

- 容器：本次 project 无残留。
- Buildx builder：本次随机 builder 无残留。
- 磁盘：根文件系统约 66% 使用率，可用空间约 48G。
- Docker：无本次验收的活动容器；全局可回收镜像和匿名卷未作一刀切清理。

## 四、边界与未完成项

本记录不替代以下生产验收：

- 目标机实际安装包、发布 digest、root 所有的环境文件和生产连接形态；
- 生产数据库只读角色、`tenant_purge` 角色和真实 `audit-rls` 核验；
- 生产备份恢复、RPO/RTO、跨副本删除边界和对象/Redis/日志保留；
- 生产探针租户、告警接收人、人工升级责任和周期调度启用；
- Telegram 外部副本及对客删除表述。

因此本记录结论为：**Docker-only 非生产验收通过；WP-8 整体和 G5 仍未通过。**

[report]
task: WP-8-docker-only-nonproduction-acceptance
path: ok
gates: pass
escalation: ->
