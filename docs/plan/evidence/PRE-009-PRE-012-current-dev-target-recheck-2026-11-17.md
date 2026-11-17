# PRE-009/PRE-012：当前 dev 候选双目标机复验（2026-11-17）

任务：`PRE-009-PRE-012-current-dev-target-recheck-20261117`。

复验基线：`cloud/dev@c9e5365ed0e2c23633ad7bfbfeb1b42ac20fb202`。
前置 `dev` CI：`43901528246`，成功。
候选流水线：`43906198009`，`workflow_dispatch`，成功。
候选 Release：`candidate-43906198009-a1-c9e5365ed0e2`，GitHub Pre-release。
候选版本：`0.1.0-candidate.43906198009.1.c9e5365ed0e2`。

判据出处：`docs/plan/proposals/PRE-009-dev候选二进制验收包.md` §三；
`docs/plan/proposals/PRE-012-CentOS7原生验收兼容收口.md` §三；
`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8。

目标机均使用 `scripts/tgm-native-target-acceptance.sh auto`，下载来源固定为
Release 直链，候选来源提交通过精确 SHA 校验。以下结果来自维护人回传的脱敏摘要，材料目录
由目标机保留。

## 一、CentOS 7

| 检查项 | 结果 |
|---|---|
| 下载 | `direct=7 api=0 gh=0 reused=0 auth=none` |
| 候选签名 | `pass` |
| provenance 来源证明 | `pass` |
| 二进制检查 | `pass` |
| 原生服务预检 | `expected-fail` |
| 预检拒绝原因 | `systemd lacks +SECCOMP for RestrictAddressFamilies` |
| 候选安装 | `not-run` |
| 生产服务 | `untouched` |

材料目录：`/home/tgmtest/tgm-candidate.DAMHvx`。
临时诊断目录：`/tmp/tgm-native-target-acceptance.TSqh26`。

该结果符合现有 fail-closed 安全边界。CentOS 7 的 systemd 未声明当前 unit 所需的
`+SECCOMP`，因此原生服务预检拒绝继续；没有删除 `RestrictAddressFamilies`、放宽 unit
安全配置或触碰生产服务。二进制、签名和来源检查通过，不等于 CentOS 7 原生 systemd
服务满足部署安全基线。

## 二、Rocky 9.8

| 检查项 | 结果 |
|---|---|
| 下载 | `direct=7 api=0 gh=0 reused=0 auth=none` |
| 候选签名 | `pass` |
| provenance 来源证明 | `pass` |
| 二进制检查 | `pass` |
| 原生服务预检 | `pass` |
| 候选隔离安装 | `pass` |
| journal 脱敏检查 | `pass` |
| 生产服务 | `untouched` |
| 数据库迁移 | `non-production acceptance only` |

材料目录：`/home/tgmtest/tgm-candidate.MO4Mdb`。
临时诊断目录：`/tmp/tgm-native-target-acceptance.EzGixc`。

当前候选在 Rocky 9.8 的签名、来源、二进制、原生预检、候选隔离安装和非生产运行时验收
均通过。数据库迁移和依赖服务限定在临时验收环境；本结果不代表生产数据库、生产备份恢复、
生产 timer、真实租户清理或生产 Telegram 链路已验收。

## 三、裁决与边界

- 当前候选的签名发布、来源证明和 Rocky 9.8 非生产目标机验收证据已补齐。
- CentOS 7 的二进制兼容和材料验证通过；其原生服务因 `-SECCOMP` 继续保持预期拒绝。
- PRE-009 不把 CentOS 7 的预期拒绝改写为原生服务通过；其整体状态仍由候选验收判据裁决，
  不据此宣布 WP-8 或生产就绪。
- PRE-012 的 Python 3.6、timer 能力判断和安全字段拒绝回归已在本地通过；目标机的
  `-SECCOMP` 能力缺口仍保留，不能通过删除加固项或绕过预检解决。
- 不改变 G0、G1、G2、G3、G4 或 WP-8 的总体状态；不重复 Rocky 验收，不主动制造或重复
  FloodWait，不启用生产 timer，不删除真实租户。

```text
[report]
task: PRE-009-PRE-012-current-dev-target-recheck-20261117
path: blocked
gates: fail
escalation: docs/plan/proposals/PRE-012-CentOS7原生验收兼容收口.md
```
