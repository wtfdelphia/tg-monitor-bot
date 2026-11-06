# WP-8 Rocky 9.8 rule 运行时验收（2026-11-06）

任务：`WP8-rocky-rule-runtime-evidence`。

基线：`cloud/dev@c9570a543078dcf75e4cf90d96fd02cfdb1c2698`，PR #94 合入提交。
候选 Release：`candidate-42137179098-a1-c9570a543078`，候选构建 run `42137179098`。
归档 SHA-256：`49ee4e2a6504fde31db332ebb5885c1ac061ad9dc2d8f93f278f55935abe64a7`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8、§三 G3/G4；
`docs/design/ops/00-部署与运行时.md`；`docs/design/ops/01-上线就绪与运维.md`；
`docs/plan/proposals/PRE-009-dev候选二进制验收包.md`。

本记录只覆盖 Rocky 9.8 上候选二进制的 `rule` 角色隔离运行时，不代表所有原生服务、
生产部署、WP-8 整体或 G3/G4 通过。

## 一、来源与主机

| 项目 | 结果 | 证据 |
|---|---|---|
| 候选构建 | `pass` | GitHub Actions run `42137179098`，来源提交与 `cloud/dev` 当前提交一致；构建及签名 job 成功 |
| 候选材料 | `pass` | GitHub Pre-release 含归档、SHA-256 清单、Sigstore bundle、provenance 及其 bundle、SBOM、BUILDINFO |
| Rocky 来源校验 | `pass` | 目标机报告签名、provenance、归档摘要、二进制、原生预检及候选隔离安装均通过 |
| Rocky 主机 | `pass` | Rocky Linux 9.8 x86_64、glibc 2.34、systemd 252、`+SECCOMP` |
| CentOS 7 对照 | `expected-fail` | 同一候选签名、provenance、摘要和二进制检查通过；systemd 219 `-SECCOMP`，原生服务预检按策略拒绝，未安装服务 |

候选来源验证报告中的归档摘要为上表所列值。CentOS 7 的拒绝是预期安全边界，不能记为
CentOS 7 原生服务通过，也不通过删除 unit 加固项绕过。

## 二、隔离运行时范围

使用脚本 `/tmp/tgm-rocky-runtime-acceptance.sh`，SHA-256：
`5d25a73febc6bba8a7e19477aa3aae2142c1d07a8d7843e5526d640843c4033e`。
脚本由 Rocky 目标机普通用户执行。测试使用本次随机命名 Docker Compose project、临时
PostgreSQL、Redis、候选二进制及随机命名的 user unit；不连接生产服务或真实 Telegram，
不读取生产凭据。退出清理仅针对脚本创建的 project、卷、网络、unit 和临时目录。

| 检查 | 结果 | 实测输出 |
|---|---|---|
| 临时 PostgreSQL / Redis | `pass` | 两个服务均通过健康检查 |
| 候选迁移 | `pass` | `tgm migrate` 应用 57 个版本 |
| RLS 审计 | `pass` | 28 条断言、0 条报红；审计说明仍要求以反向测试证明隔离语义 |
| 离线配置检查 | `pass` | `rule` 原生环境文件校验通过，未连接外部服务 |
| user unit 解析与启动/停止 | `pass` | `systemd-analyze --user verify` 通过；启动 active、停止 inactive |
| 进程凭据边界 | `pass` | 运行进程未继承脚本检查的 owner、管理数据库及 AWS 凭据变量 |
| SIGTERM 停止 | `pass` | unit 停止后为 inactive/dead |
| SIGKILL 自动恢复 | `pass` | `NRestarts=1`，新 PID 与旧 PID 不同 |
| 缺失环境文件 | `pass` | unit 进入 failed；随后单独读取 journal 检查 |
| StartLimit 限速 | `pass` | 错误 profile 下 `NRestarts=3`，`StartLimitBurst=3` 生效 |
| 恢复有效配置 | `pass` | unit 再次进入 active/running |
| 凭据日志匹配检查 | `pass` | 有权限读取对应时间窗 journal 共 33 行；未匹配到测试连接串或密码模式 |
| 清理 | `pass` | 脚本报告自动清理本次测试创建的资源；生产服务与生产数据未触碰 |

脚本自身无权限读取 user journal，因此脚本内的 journal 检查曾显示空日志；该结果不作为
日志脱敏通过证据。随后以有权读取 system journal 的方式检查到 33 行，并对本次测试连接串、
密码模式执行匹配，结果为 `journal_redaction=pass`。这项结果只覆盖已检查日志时间窗与
指定测试值，不代表所有日志路径、所有秘密类别均已穷尽验证。

此前一次脚本运行因 `reset-failed` 对未加载 unit 返回非零而提前退出，没有最终报告；修复
该脚本控制流后，本记录只采纳生成完整 `[report]` 的成功运行。

## 三、裁决边界

- 本次通过只证明候选 `c9570a5` 在 Rocky 9.8 上 `rule` 角色的隔离数据库和用户级 systemd 运行形状。
- 不证明其他应用角色、系统级清理 timer、生产环境文件权限、生产告警、真实备份恢复或真实探针租户已经通过。
- 未连接生产 PostgreSQL、Redis、KMS、对象存储或 Telegram；未执行生产迁移、生产删除或启用生产 timer。
- CentOS 7 原生服务仍因 `-SECCOMP` 被安全预检拒绝；Docker-only 验收属于另一部署形态。
- 本记录不宣布 WP-8、G3、G4 或 G5 通过。

```text
[report]
task: WP8-rocky-rule-runtime-evidence
path: ok
gates: not-run
escalation: docs/plan/proposals/WP-8-production-ops-acceptance.md
```
