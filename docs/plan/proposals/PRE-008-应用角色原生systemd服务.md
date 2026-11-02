# PRE-008：应用角色原生 systemd 服务

任务 id：`PRE-008-native-systemd-app-roles`。

状态：实施与本地验收已完成；目标 CentOS 7 实机验收待完成。PR #78、#79 已合入 `dev`；安装集成以 PRE-007 的最终合并内容为基线。PRE-007 只打包现有 cleanup unit/timer；本提案为常驻应用角色定义 systemd 服务工件、root 与普通用户安装方式及非生产验收，不自动安装到生产机，不启动服务，不执行迁移。

实施授权记录（2026-11-02）：维护人在 PR #78、#79 合入 `dev` 后明确要求开始实施本提案。
实施基线为 `4fe63d828def54fae08021af983fccf781f44c28`，不改变生产操作授权边界。
本地实现和验证记录见 `../evidence/PRE-008-应用角色原生systemd实施-2026-11-02.md`；
目标 CentOS 7 的实际 manager、沙箱和启停验收尚未提供，不能据此宣布整体出口通过。

## 一、背景与目标

仓库已有单一 `tgm` 二进制和六种 `tgm serve --role`：`control-plane`、`bot-gateway`、`mtproto-worker`、`fanout`、`rule`、`delivery`。生产部署文档规定这些角色分进程运行，但原生发布包当前没有对应的常驻服务单元。

本提案补齐 systemd 部署工件与安装流程，使管理员可以显式安装、配置、检查、启动、停止和回滚应用角色进程；同时保留 Docker 为另一种独立部署方式。它不把“unit 可解析”或“进程能启动”表述为生产就绪。

判据出处：`docs/design/ops/00-部署与运行时.md` §一、§二、§六；`docs/design/eng/00-工程约定.md` §一；`docs/design/eng/01-配置与Secret.md` §二至§四；`docs/plan/proposals/PRE-007-二进制生产发布.md` §五至§七。

## 二、服务范围

首片覆盖 CLI 已支持的六个常驻角色，每个角色单独定义服务实例，不把多个角色合并到一个 systemd unit：

| 角色 | 命令 | 配置边界 |
|---|---|---|
| `control-plane` | `tgm serve --role control-plane` | 仅 control-plane 白名单配置；不持有 owner 或 cleanup 凭据 |
| `bot-gateway` | `tgm serve --role bot-gateway` | 仅 bot-gateway 白名单配置；不持有 owner 或 cleanup 凭据 |
| `mtproto-worker` | `tgm serve --role mtproto-worker` | 仅该角色配置；session 与平台 KEK 权限按现有配置规范提供 |
| `fanout` | `tgm serve --role fanout` | 仅 fanout 白名单配置；媒体清理连接和 S3 凭据不得扩散到其他角色 |
| `rule` | `tgm serve --role rule` | 仅 rule 白名单配置，不附加 KEK、S3 或 Telegram 凭据 |
| `delivery` | `tgm serve --role delivery` | 仅 delivery 白名单配置；平台 API、KEK 和 S3 凭据不得复用给其他角色 |

角色名称、命令与所需环境变量必须从 CLI 和 `docs/design/eng/01-配置与Secret.md` 派生，不能在 unit 中维护第二套配置规则。实现时需重新核对 `dev` 上的最新配置，表格不替代配置清单。

## 三、安装模式与身份边界

### 3.1 root 系统级安装

- root 仅用于安装二进制链接、system unit 和专用服务账号；六个应用进程均以非 root 身份运行。
- 每个角色使用独立的系统用户和组，不共享 HOME、可写目录或凭据文件。默认禁止登录，不授予 sudo、Linux capabilities 或额外组权限。
- 每个角色使用独立 root 所有的环境文件，权限限制为该服务身份可读；内容按角色白名单生成，禁止放入 `TGM_DB_URL_OWNER`、其他角色凭据和 `DATABASE_URL`。
- 配置示例不含可用 Secret。安装器不得自动生成密码、复制调用者环境、启动服务或启用服务。
- unit 和环境文件冲突时拒绝覆盖；服务文件变更须由 root 明确确认并保留可审查的原文件。

### 3.2 普通用户级安装

- 将二进制和 unit 安装到调用者的用户目录与 `systemd --user` 配置目录，不请求提权，不修改系统 unit、系统用户或系统服务状态。
- 六个进程由同一个 Unix 用户运行，因此此模式**不提供角色间的操作系统凭据隔离**。文件权限只能防止其他本机用户读取，不能防止同一用户下的角色进程彼此读取配置。配置和文档必须明确此限制。
- 不假定目标机已启用用户级 systemd manager、linger 或登录会话外持续运行。安装前检测用户 manager 是否可用；不可用时拒绝宣称具备后台常驻能力，并给出只运行前台进程或使用 root 系统级安装的明确结果。
- 普通用户模式适用于开发、隔离验收和用户明确接受同 UID 风险的单机运行；不作为多租户生产环境等同于 root 模式的替代品。

两种模式均由用户显式选择。不能因进程以普通用户运行就把 system-level 与 user-level 服务称为同等安全边界。

## 四、unit 与运行行为

1. 每个角色至少提供独立的 service unit；可以用模板减少文件重复，但角色到 unit、用户、环境文件和二进制参数的映射必须静态、可审查，不允许 shell 拼接或从任意环境变量生成 `ExecStart`。
2. 常驻服务使用前台进程和 `Type=simple`，除非实现另有已验证的通知协议。服务以绝对路径调用当前安装版本，不依赖交互 shell、PATH、工作目录或登录配置。
3. 停止时向主进程发送 SIGTERM，由现有应用退出路径收敛任务；`KillMode` 和停止超时要与仓库已有的 30 秒优雅退出实测对齐。超时后 systemd 可终止该 unit 的整个控制组，不得留下脱离管理的子进程。
4. 失败重启策略应限制重启频率并保留 journal 诊断；不得使用无限快速重启配置掩盖错误。验收需要覆盖正常退出、配置错误、依赖暂时不可达、连续失败限速及人工停止后的不重启行为。
5. unit 仅表达到网络就绪的启动顺序，不伪造本机 PostgreSQL、Redis、PgBouncer 或对象存储依赖；这些服务可能是远端服务。应用角色之间不增加未经架构定义的强依赖。
6. 服务日志进入 journald，不写应用自管理日志文件。日志不得包含 token、连接串、Telegram session、KEK 或 KMS 凭据。
7. unit 的沙箱与系统调用限制必须以 CentOS 7 目标机实际 systemd 版本验收。对目标机不支持的指令，不得静默删掉后仍报告等价加固；需记录差异并由提案批准范围处理。

## 五、配置、更新与回滚

- root 模式沿用 PRE-007 的版本目录和当前版本链接。更新先验证签名、包内校验、动态依赖、所有 unit 语法与 `tgm serve --role <role> --help`（若该 CLI 无角色级 help，则用现有 help 与非生产启动验证替代），再由操作者安排逐角色重启。
- user 模式使用用户目录中的独立版本目录和当前版本链接；unit 不绑定具体版本目录，使链接切换后可按操作步骤逐角色重启。
- 安装器只安装和校验，不写入生产配置，不运行 `tgm migrate`、`tgm audit-rls`，不执行 `systemctl start/enable` 或 `systemctl --user start/enable`。
- 配置文件由操作者单独创建；变更时先校验权限和角色白名单，再逐角色重启。文档提供 `systemctl` 与 `systemctl --user` 的检查、日志、停止和回滚命令。
- 二进制回滚不回滚数据库迁移。已前滚 schema 后，旧版兼容性必须按恢复流程判定；unit 安装器不得提供会暗示数据库一并恢复的操作。

## 六、非生产验收与出口判据

验收只使用隔离测试数据库、测试 Bot/测试账号和非生产 Redis/S3；不连接生产服务、不运行真实租户数据，也不启用目标机生产服务。

1. 六种角色的 unit 均能由目标机 `systemd-analyze verify` 解析；在目标机采集 `uname -m`、glibc、OpenSSL 和 `systemd --version`，不得以开发机结果替代 CentOS 7 目标机证据。
2. root 安装演练确认二进制由各专用非 root 身份运行；普通用户安装演练确认不调用 sudo、不修改系统级状态，并明确记录同 UID 凭据隔离缺失。
3. 每个角色的环境文件权限、属主、变量白名单和 secret 缺失/多配拒绝均有断言；尤其证明常驻角色不持有 owner、cleanup 及不属于该角色的凭据。
4. 每个角色在隔离依赖下启动成功；SIGTERM 后在已验证的时间内退出，停止后无遗留子进程。启动成功只验证运行链，不证明真实 Telegram、KMS、S3 或生产数据路径已验收。
5. 覆盖服务失败重启限速、人工停止、环境文件丢失或权限过宽、旧版回滚及配置错误；日志检查确认没有 secret。
6. 安装、升级与回滚不自动迁移数据库、不启动或启用任何服务；记录每个命令造成的状态变化，确认失败安装不会切换当前版本。
7. root 与 user 模式的安装、卸载和回滚只操作本任务明确创建的路径；不得删除既有 unit、配置、版本目录或系统用户。
8. 所有 unit 在目标 CentOS 7 的 systemd 上实测；`systemd-analyze verify` 的通过不能替代目标机启动、停止和重启演练。
9. 完成代码后运行 `scripts/ai-driver/ai-gates.sh`，PR CI 的 `static` 与 `database` 检查通过；按仓库规则检查并清理本任务专属构建缓存和临时 Docker 残留。

## 七、不在本提案范围

- 不改数据库结构、迁移、RLS、GRANT、API 或运行角色的凭据白名单。
- 不将任何应用进程以 root 身份运行；root 仅负责系统级安装和服务账号管理。
- 不自动配置生产 Secret，不迁移数据库，不启动/启用/停止生产服务，不连接生产 Telegram、KMS、S3 或租户数据。
- 不改变 Docker 镜像发布路径，不把 systemd unit 用作 Docker 容器内的进程管理器。
- 不配置主机防火墙、反向代理、TLS 证书、备份、告警接收人或生产容量参数；这些仍按现有生产上线验收流程处理。
- 不将“六个服务可启动”作为 WP-8、G4/G5 或生产上线通过的替代证据。

## 八、批准后实施顺序

1. 在批准基线上重核六角色 CLI、配置白名单、SIGTERM 行为和 PRE-007 包结构。
2. 先完成目标 CentOS 7 的 systemd 版本及用户级 manager 能力采集；不支持的用户级能力须按本提案限制处理，不绕过检测。
3. 实现六角色 unit、环境文件示例、root/user 安装与检查流程；不修改生产 Secret，不触发服务启停。
4. 在隔离环境完成权限、配置拒绝、启动、停止、重启限速、失败回滚和旧版兼容性演练。
5. 记录脱敏证据、跑完整本地门禁并提交一个 PR；CI 通过后由维护人决定是否合并。
6. 生产安装、配置 Secret、数据库迁移和逐个角色启用须另行安排并单独授权，不包含在本提案批准内。

[report]
task: PRE-008-native-systemd-app-roles
path: ok
gates: pass
escalation: -
