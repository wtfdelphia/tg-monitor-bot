# WP-8：Rocky 六应用角色非生产验收提案

任务 id：`WP8-rocky-app-role-acceptance`。

状态：本片验收完成；六角色运行时与 system journal 脱敏均通过。基线为
`dev@eb4f5aaf72e35885883423460b0038f5c6e9649e`；对应 `dev` CI run `42356267491` 成功，
候选构建 run `42360824040` 已手工触发。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8；
`docs/plan/proposals/PRE-008-应用角色原生systemd服务.md` §六；
`docs/plan/proposals/PRE-009-dev候选二进制验收包.md`；
`docs/plan/evidence/WP-8-Rocky9-rule运行时验收-2026-11-06.md`。

## 一、范围与原因

现有 Rocky 目标机证据只覆盖 `rule` 的临时用户级 unit，且候选包基于旧提交。本片使用与当前
`dev` 一致的签名候选包，验收 `control-plane`、`bot-gateway`、`mtproto-worker`、`fanout`、
`rule`、`delivery` 六个应用角色。复用现有 `native_systemd.rs` 和
`check-native-service-runtime.py` 的配置形状及隔离检查，不改生产进程、systemd 模板或安装器。

候选包不包含源码和 `scripts/init/01-roles.sql`。目标机需要一份独立、来源明确的验收辅助目录，
其中包含上述初始化脚本及本片验收工具；辅助目录不属于候选包，不得改变归档、签名或 provenance。

## 二、实施范围

1. 新增普通用户执行的验收脚本，拒绝 root 执行，要求显式传入候选二进制路径及预期完整来源提交。
2. 启动随机命名的 PostgreSQL、Redis 与 MinIO 临时 Compose 环境，初始化受限数据库角色并走正式
   `tgm migrate` 路径；核对 `audit-rls`，不把静态审计解释为隔离语义证明。
3. 为六角色生成开发模式、最小权限、临时端口的环境文件，启动随机命名的用户级 unit。
   数据库使用合成数据；控制 Bot、数据 Bot 与 OIDC 配置只用于本地进程启动，不发起外部登录或轮询。
4. 每角色执行离线配置检查、unit 加载参数、启动/停止、进程 UID、环境白名单、SIGTERM、SIGKILL
   自动恢复、无效配置失败与日志脱敏检查。普通用户无法读取 system journal 时记为 `not-run`；
   通过临时最小 sudoers 规则启用 `--system-journal` 后，按本次随机 unit 做只读日志脱敏检查。
   失败保留脱敏诊断并停止，不跳过失败角色。
5. 仅清理本脚本随机命名的 project、临时卷、unit、用户 manager 和环境目录。不得停止或删除
   已有 `tgm-pre008-acceptance` 环境，不执行 `docker system prune --volumes`。

## 三、系统级清理边界

本片不把 `tgm-tenant-purge.timer` 或其他清理 timer 的 `verify`、`calendar` 结果写成实际运行通过。
系统级清理角色、root 服务账号、root 所有的 `/etc/tgm/*.env`、一次性清理服务实际执行与 timer
启停属于另一片，必须先提供专用 Rocky 主机/快照、合成数据、允许创建和删除的测试用户清单及操作窗口。

即使非生产环境批准执行清理，也只允许无真实数据的空库或明确命名的探针租户；不得启用持久 timer，
不得执行真实租户物理删除，不得把主库清理结果扩展为跨副本删除。

## 四、验证与交付

- 先在本地运行 Python 语法、脚本静态检查和复用测试；若修改保护脚本，数据库与完整门禁由驱动裁决。
- 候选 workflow 成功后记录候选 tag、来源提交、CI run 和 Release 资产，不创建稳定 tag 或正式 Release。
- 向维护人提供 Rocky 手动命令，回显只包含主机版本、角色状态、迁移/审计计数和脱敏错误；
  不回显 token、api_hash、session、密码、完整连接串或消息正文。
- 目标机补测已覆盖 `control-plane`、`bot-gateway`、`mtproto-worker`、`fanout`、`rule`、`delivery`；
  六个角色的 `journal_redaction` 均为 `pass`，本次临时 Compose project 已清理。
- 按实际结果新增证据文件。未运行、失败和超范围项分别记录，不把六角色通过扩大为 WP-8、G3、G4、
  系统级清理或生产部署通过。

## 五、停止条件

出现候选来源不一致、需要真实凭据、需要外呼 Telegram、需要修改生产代码或 unit、root 权限要求、
真实租户数据访问、清理 timer 启用要求或无法确认归属的 Docker 资源时，立即停止并升级。

```text
[report]
task: WP8-rocky-app-role-acceptance
path: ok
gates: static-pass
escalation: ->
```
