# WP-8：租户物理清理运行入口与调度提案

任务 id：`WP-8-tenant-purge-runtime`。

状态：已批准，候选枚举安全边界已先行完成。PR #63 已将 `public.tenant_purge_batch(bigint, text)`、受限
`tenant_purge_batcher` 角色、批次日志和共享阻塞重试路径合入 `dev`。本提案申请补齐
调用层和部署工件，不申请改变已合入迁移、不申请真实生产租户删除，也不据此宣布 WP-8
通过。

基线与判据：`docs/plan/proposals/WP-8-tenant-purge-batches.md` §三至§六；
`docs/plan/evidence/WP-8-租户物理清理终结入口-2026-10-29.md`；
`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8；
`docs/design/spec/04-数据生命周期与容量.md` 总则、§一、§1.3；
`docs/design/spec/05-安全与租户隔离.md` §3.3；
`docs/design/eng/01-配置与Secret.md` §一；
`docs/design/eng/04-CI门禁.md` C16、C17。

## 一、为什么需要单独提案

当前数据库批处理入口已经可以按固定类别推进一个已注销租户，但仓库还没有一个可审查
的调用层来完成以下动作：

1. 枚举已过恢复窗口的 `deleted` 租户；
2. 按固定类别顺序重复调用批处理函数；
3. 在私有残留归零后调用现有 `tenant_purge_finalize(bigint)`；
4. 对 `locked`、共享阻塞、在途阻塞、未到期和失败结果返回非零，并保留可重跑语义；
5. 以独立运行凭据接入 systemd，而不把物理清理权限交给业务服务或普通留存清理进程。

直接把这些动作塞进现有 `cleanup-tenant` 会混淆两套不同的完成语义：普通留存按套餐
期限清理，物理注销按租户恢复窗口和共享引用清理。应使用独立子命令、独立配置边界和
独立 systemd 单元。

## 二、拟实现范围

### 2.1 CLI

新增只面向已注销租户的命令，建议形状为：

```text
tgm purge-deleted-tenants [--tenant-id <id>] [--max-tenants <n>]
```

具体命令名在实现前固定一次；不得提供任意表名、任意类别、SQL 片段或可由调用者传入的
删除条件。建议语义如下：

- 不带 `--tenant-id` 时，只枚举 `status = 'deleted'` 且 `deleted_at <= now() - interval '30 days'`
  的租户，并以稳定主键顺序限制本轮扫描数量；
- 带 `--tenant-id` 时，只处理指定租户，仍必须重新验证状态和 30 天宽限期；
- 每个租户按 `identity_dependents`、`rule_dependents`、`configuration`、`notifications`、
  `terminal_delivery`、`tenant_events`、`tenant_audit` 的固定顺序运行，类别返回非成功结果时
  停止该租户后续类别；
- 所有私有类别返回 `empty` 或 `deleted` 后，调用 `tenant_purge_finalize(bigint)`；
- 只有 finalizer 返回 `deleted` 才把该租户计为完成；批处理返回 `deleted` 不得被 CLI 汇总为
  根行已删除；
- `locked`、`blocked_shared_reference`、`blocked_inflight`、`not_due`、`not_deleted` 和
  `failed` 进入非零退出或明确的部分失败汇总，下一轮可以安全重跑；
- 标准输出只包含租户数量、类别、结果、批次数和阻塞计数，不输出消息正文、session、密文、
  对象 key 或完整连接串。

CLI 不在一个长事务中包住多个批次；每次数据库函数调用保持现有短事务和日志原子性。
进程中断后不回滚已提交批次，下次执行从当前数据库状态继续。

### 2.2 独立凭据与配置

新增专用配置变量，建议为 `TGM_DB_URL_TENANT_PURGE`，只允许直接登录
`tenant_purge`。配置检查必须：

- 要求 `TGM_PROFILE`、`TGM_DB_URL_TENANT_PURGE` 和可选 `RUST_LOG`；
- 拒绝 `DATABASE_URL`、`TGM_DB_URL_OWNER`、业务连接串、其他清理连接串、KMS、Telegram、
  Redis 和对象存储凭据混入；
- 拒绝通过 PgBouncer transaction pooling 承载需要事务级 advisory lock 的调用；
- 不在常驻业务角色、`tenant_cleanup` 或 `tenant_lifecycle` 的环境文件中复用该连接串；
- 启动时以数据库返回的 `current_user`、事务隔离级别和角色属性做运行前检查，失败即拒绝执行。

若实现发现现有 `tenant_purge` 权限不足，必须另写安全面/迁移补充提案并停止，不能顺手扩大
角色权限。不得修改已应用的 `0056` 或 `0057–0060` 迁移。

### 2.3 systemd 工件

新增独立的 oneshot service、扫描 timer、示例环境文件和安装说明：

- service 使用独立系统用户 `tgm-tenant-purge`，`UMask=0077`、`NoNewPrivileges=true`、
  `ProtectSystem=strict`、`ProtectHome=true`、`PrivateTmp=true`、无 Linux capabilities；
- timer 默认每日运行，使用 `Persistent=true`；具体频率只作为调度默认值，不改变数据库
  30 天宽限期和批处理结果语义；
- 生产启用前先手工运行指定租户的无破坏预检，再运行隔离探针租户，最后才启用 timer；
- 停止时先停 timer，再等待或停止 service，不删除数据库、Docker 具名卷或备份；
- systemd 样例和本地临时 timer 只能证明工件可解析与命令可启动，不能替代生产验收。

## 三、验收判据

实现后至少验证：

1. 空扫描不触碰未到期、`active`、`suspended` 或不存在租户；
2. 指定租户仍重新检查 `deleted_at` 和 30 天边界，不能由命令参数绕过；
3. 同一租户重复执行只产生幂等结果，批处理和 finalizer 的短事务边界保持不变；
4. 任一类别返回锁冲突、共享引用、在途任务或失败时，后续类别不误跑，进程退出状态可供
   systemd 告警使用；
5. 阻塞消失后下一次执行继续推进，只有根行真正删除才计为完成；
6. 限制 `--max-tenants` 不会把单租户的类别批次上限扩大到 5000 以上；
7. 配置污染、错误角色、错误事务隔离级别、PgBouncer 连接和缺失环境变量均在启动前拒绝；
8. systemd 单元通过 `systemd-analyze verify`，timer 日历通过 `systemd-analyze calendar`；
9. 受限连接无法读取密文/session/消息正文/对象 key，无法直接删除表行、TRUNCATE、建对象或
   提升角色；已有 C16/C17 和 `audit-double-pass` 结果保持通过；
10. 运行专项测试、`cargo fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、
    `cargo deny check`、`cargo sqlx prepare --workspace --check`、workspace 测试、迁移幂等、
    `audit-rls`、`audit-double-pass` 和完整本地门禁；
11. 收尾检查 `df -h`、本片专属 `target/` 和新增 Docker 残留，仅清理已确认归属且不在使用的资源。

## 四、不包含与停止条件

本提案不包含：

- 任何真实生产租户的删除；
- Redis、数据库备份、快照、恢复副本、宿主机日志或 Telegram 已投递内容的删除；
- 对象存储全局副本删除或底层介质安全擦除；
- 同意文本归档、举报入口、第三方内容撤回和隐私请求处理 API；
- 修改已有迁移、放宽 A8、改变 RLS/GRANT，或新增 `SECURITY DEFINER` 函数。

若实现需要新增表、迁移、安全策略、角色权限、审计断言或 CI 门禁，按 `AGENTS.md` §五
重新升级并停止，不在本任务中自行扩大范围。生产备份副本留存与恢复后防止已删除数据重新
可用的流程仍须单独定案，不能把本命令的成功写成全副本删除完成。

本提案批准后，在独立 worktree 实现；本地门禁和专项验收通过后，再申请按一个提交提交、
推送并开向 `dev` 的 PR。PR CI 和合入仍分别等待维护人裁决。

[report]
task: WP-8-tenant-purge-runtime
path: escalated
gates: not-run
escalation: docs/plan/proposals/WP-8-tenant-purge-runtime.md
