# WP-7：目标自检新增列的审计反证恢复

任务 id：`WP-7-target-probe-audit-restore`。

状态：2026-10-23 维护人批准 C2 恢复清单与 `ctutils` 兼容补丁更新，已实施并通过本地完整门禁。
仅调整 C2 撤销清单及 `eng/04` 对应说明；不修改审计 SQL、预期报红集合、快照要求或 CI。

## 一、实测问题

已批准的 `WP-7-target-probe` 在 `identities` 增加持久冷却列 `target_probe_not_before`，
迁移仅授予该列 UPDATE，仍禁止修改 `tenant_id`，也未授予表级 UPDATE。

完整门禁中的迁移、迁移幂等、测试和 RLS 审计通过，但 double-pass 在 C2 恢复后的目录快照比对失败：

```text
✗ C2 撤销有残留 —— 目录快照与第一遍不一致：
< COLACL|identities|target_probe_not_before|app_user|UPDATE
```

原因可在脚本复核：C2 先用表级 UPDATE 制造违规，撤销时收回表级权限，再硬编码恢复原有列级 UPDATE。
该恢复清单未包含新增列，因此审计配置本身没有报红，但目录快照正确发现缺失权限，阻止假绿。
停止后已在本地恢复迁移规定的该列授权，`tenant_id` 仍不可 UPDATE；没有降低快照要求或跳过该卡口。

## 二、请求批准的最小修复

只修改 `scripts/audit-double-pass.sh` 的 C2 撤销 SQL，将 `target_probe_not_before` 加入恢复清单，
与已批准迁移的实际权限一致。不修改破坏 SQL、预期报红集合、快照内容、断言或退出码。

同步更新 `docs/design/eng/04-CI门禁.md` 对恢复清单的说明，保留“恢复后目录逐字一致”的验收要求。
不改已应用迁移，不回退为表级 UPDATE，不给 `tenant_id` 新权限，不修改 CI。

批准后先复跑 double-pass，确认 C2 仍恰好报红且撤销后快照一致；随后重跑完整门禁，
证据继续区分本地自动化、真实浏览器与 Telegram 真机。提交、推送和 PR 另行授权。

## 三、同轮发现的依赖门禁阻塞

后续静态复跑的 `cargo deny` 在现有锁文件的 `ctutils 0.4.2` 上报告 `RUSTSEC-2026-0385`。
本机已获取的 RustSec 记录日期为 2026-10-23，修复版本为 `>= 0.4.4`；内容涉及非恒定时间的掩码生成与 u128 比较。
首轮完整门禁的 deny 曾通过，因此不能用首轮结果覆盖后续失败。

本轮按批准范围仅将 `Cargo.lock` 中的该传递依赖更新至 `0.4.4`，不添加忽略项，不关闭漏洞检查。
`cargo deny check advisories` 已通过；该依赖问题与 C2 分开处理，二者均已在本轮重新验证。

## 四、实施结果

- `scripts/audit-double-pass.sh` 的 C2 撤销清单已补回 `target_probe_not_before`，未修改破坏 SQL、预期报红集合、快照内容、断言或退出码。
- `docs/design/eng/04-CI门禁.md` 已同步说明旧列与新增冷却列必须一并恢复，目录快照仍要求逐字一致。
- `Cargo.lock` 仅将 `ctutils` 从 `0.4.2` 更新至 `0.4.4`，其他依赖集合与根依赖声明未变。
- 定向 double-pass 通过：18 条断言无报红；C2 破坏恰好报红；恢复后 640 行目录快照逐字一致。
- 完整 `scripts/ai-driver/ai-gates.sh` 通过：fmt、clippy、离线构建、deny、newtype、OpenAPI、驱动回归、迁移、sqlx 检查、迁移幂等、workspace 测试、RLS 审计及 double-pass 均通过。workspace 测试为 372 passed / 0 failed / 21 ignored；本地 cargo-deny 仍提示与 CI 固定版本的既有版本漂移（本地 0.20.2、CI 0.19.1），不影响本轮结果。
- 显式真实 Chrome 验收通过：1 passed / 0 failed，使用本地 Bot API 桩与临时数据库；该结果不替代 Telegram 真机验收。
- 本轮未修改已应用迁移、CI、审计 SQL、`deny.toml` 或二维码工作目录，未读取真实凭据，未运行 Telegram 真机。

提交、推送、开 PR 与合并仍需另行授权。WP-7 原任务的真实 Telegram 频道验收和出口确认尚未完成，因此不据此宣布 WP-7 通过。

[report]
task: WP-7-target-probe-audit-restore
path: ok
gates: pass
escalation: ->
