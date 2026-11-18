# G3/WP-8 外部证据校验器边界修复提案

任务 id：`G3-WP8-closure-validator-hardening`。

状态：已批准。用户于 2026-11-18 明确批准实施本提案。

当前隔离工作树审查发现外部关闭证据校验器存在测量值接受过宽、畸形 JSON 导致未处理异常，
以及 G3 命令示例与 CLI 参数约束不一致的问题。本次改动限于下列范围。

判据出处：仓库根 `AGENTS.md` §五第 3 项（安全与校验门禁变更升级）；
`docs/plan/evidence/外部关闭证据输入规范-2026-11-18.md` §一、§三、§六；
`docs/plan/evidence/G3-WP8-G4统一收口复核-2026-11-18.md` §二。

## 一、范围

1. 收紧 WP-8 `rpo.measured_value` 与 `rto.measured_value` 的格式，使说明文字不能作为测量值通过；同步文档明确单位格式限制。格式校验只证明输入形状，不证明测量真实性。
2. 对 `cross_replica_retention.systems` 的每个元素先验证为非空字符串，再做必需系统集合校验；畸形输入统一转换为 `ManifestError`，不得泄漏 traceback。
3. 修正 G3 输入规范的命令示例，使其满足 CLI 对 `--g3` 与 `--wp8` 必须成对提供的约束；保留该耦合，避免 G3 汇总绕过 WP-8 子项。
4. 增加针对非测量文本、畸形副本数组、G3 参数契约的定向回归测试。

## 二、非目标

- 不放宽 G3/WP-8 关闭判据，不接受没有责任方证据引用的 `pass`。
- 不验证外部引用真实性，不连接生产，不触发恢复、清理、timer 或 Telegram 操作。
- 不改迁移、数据库权限、RLS、CI、生产代码或已有 G1/G2 裁定。

## 三、验收

- 结构合法且带单位的 RPO/RTO 时长按约定格式通过；`unknown`、`not-measured`、负数或格式错误被拒绝。
- 副本系统数组包含对象、数字、空字符串或 null 时返回结构化校验错误，不抛出 `TypeError`。
- 文档中的 G3 示例命令可直接通过 argparse 参数校验，并同时验证 WP-8 清单。
- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts.tests.test_external_closure_evidence -v` 全绿；`git diff --check` 通过。
- 保留 G3、WP-8、G4 原有未关闭状态，不因结构检查通过提升生产就绪或发布状态。

```text
[report]
task: G3-WP8-closure-validator-hardening
path: ok
gates: targeted-pass
escalation: -
```
