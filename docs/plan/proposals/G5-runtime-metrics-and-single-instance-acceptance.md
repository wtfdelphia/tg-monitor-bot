# G5：运行指标与单实例启动保护验收提案

任务 id：`G5-runtime-metrics-and-single-instance-acceptance`。

状态：维护人于 2026-11-26 批准实施（「批准提案 开始实施」）。指标与单实例实现及固定墙钟时间测试修复已合入 `dev`，合并后 CI 通过。生产抓取、告警渠道、容量实测与 Rocky 目标机子项仍未完成。

## 一、提案起草时缺口（2026-11-25）

`docs/design/ops/01-上线就绪与运维.md` §一要求上线前完成核心指标与告警、容量实测和单实例租约防误双开验证。`docs/design/eng/00-工程约定.md` §2.3 已选择 `metrics`、`metrics-exporter-prometheus` 与 `/metrics`，但当前 `Cargo.lock` 没有这两个依赖，应用源码没有 recorder 注册，Axum 路由没有 `/metrics` handler。

运维规格还规定 `/metrics` 不对公网暴露，并要求从数据库提供采集延迟、按租户投递失败率和队列深度。上述指标会跨越 HTTP 暴露面、租户数据边界和数据库读取权限，必须先固定采集角色、监听地址、label 集合与查询范围。

系统级和 user-level systemd 都提供六个角色模板。systemd 对同一 manager 中的同名 unit 提供单一 unit 生命周期，但仓库没有 Rocky 目标机重复启动行为的记录，也没有独立手工进程与 systemd role 进程并存时的专项验证。该缺口应针对首发单实例形态验收。X1–X5 多实例混沌按既有单实例决策保持 `deferred/not-run`。

初始提案基线：`cloud/dev@994ab82a2f941ec50bc37146fbfb16d5ff6758c1`，CI `45362852128` 成功。后续实现与合并后状态见第六至八节。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G5；`docs/design/ops/01-上线就绪与运维.md` §一、§三、§四、§六；`docs/design/ops/00-部署与运行时.md` §三、§六；`docs/design/eng/00-工程约定.md` §2.3；`docs/design/eng/04-CI门禁.md`。

## 二、建议范围

### 2.1 运行指标与采集

- 依 `eng/00` 已选定的 `metrics` 和 `metrics-exporter-prometheus` 接入 recorder 与 `/metrics` handler。
- 先明确由哪些 role 注册 recorder、listener 如何配置，以及 system/user unit 如何避免同机端口冲突。
- 将 metrics listener 与公网业务 listener 分开，并为默认绑定地址、允许的访问网段及配置缺失定义 fail-closed 行为。
- 按 `ops/01` §3.1 实现采集延迟、租户投递失败率和队列深度。明确采集周期、窗口、缺失样本、分母为零与重启后的行为。
- 对数据库读取优先复用已有受限角色与查询路径。若需要新 GRANT、角色、RLS 例外或改 audit SQL，停止本提案实施并另提安全方案。
- 为所有指标冻结名称、单位、label 白名单及基数约束。禁止消息正文、关键词、凭据、session、源链接或可逆内容进入 label、exposition 或错误日志。
- 不在本片启用生产抓取、告警渠道或写入生产配置。

### 2.2 单实例启动保护验收

维护人批准后，按以下可区分场景编制专项测试：

1. 在隔离 systemd manager 中连续 start 同一个 role unit 两次，确认只有一个 MainPID，第二次操作的退出与 unit 状态符合 systemd 语义。
2. 从独立 user manager 或直接命令路径启动相同 role，确认候选 binary 与现有 database lease/fencing 是否阻止同一 identity 的双持有和有效写入。
3. 强杀持租约进程，等待既有租约期限，通过 successor worker 接管，并用 stale fencing token 验证旧进程写入被拒。
4. 每个测试记录 manager、启动入口、worker/identity 数量、进程 PID 和 lease 状态。测试只使用隔离数据库与 synthetic identity，不连接 Telegram，不使用生产凭据。
5. 在 Rocky 9.8 目标机运行精确候选包时，确认 systemd 版本、unit 校验、重复 start 结果与清理情况。目标机网络不可达、SSH 身份认证失败或缺少该子项授权时，该子项保持 `not-run`。

测试报告要说明 systemd 同名 unit 的保证范围和 unit 外进程路径的结果，避免把 manager 内重复 start 的行为扩大为主机上任意进程都不可重复。

## 三、必须先由维护人确认

- `metrics-exporter-prometheus` 是否以已有工程约定版本实施，是否允许增加依赖。
- 哪个 role 运行 exporter，systemd system/user 部署下的私网 bind address 与端口如何配置。
- 指标中的 tenant 维度采用受控租户标识、预聚合还是其他既有安全表示，数据读取由哪个已有受限身份承担。
- 指标采集在数据库不可用、租户数量过大、label 基数超限时的行为和停止条件。
- 单实例测试覆盖同名 systemd unit、跨 manager 和直接进程三种入口的范围。
- 是否授权 Rocky 目标机上启动隔离候选服务进行重复启动测试。授权只覆盖列明的 candidate、role、窗口、环境文件和测试数据库，不包括生产数据库或外部告警。

## 四、验收标准

- CI 全部必需门禁通过；新增实现有相应反向测试。
- `/metrics` 只绑定批准的非公网接口，缺失或过宽配置被拒绝。
- 指标名称、单位和 label 与批准清单一致，未知 label 和内容数据不会出现。
- 隔离受限角色查询只读取指标所需字段，不扩大 GRANT/RLS 权限。
- 指标采集失败不会阻塞业务服务启动或改变投递语义，相关错误会以脱敏形式报告。
- systemd 同名 unit 重复 start、unit 外第二进程竞争和租约 fencing 分别记录结果，不混为一个判据。
- Rocky 目标机复验绑定精确候选版本与源码提交。目标机检查未运行时，不把 CI 或本地 systemd 测试记成目标机通过。

本提案完成不代表 G3、G5、WP-8 或生产验收通过。生产指标抓取、外部告警和生产服务启用仍需各自的运维授权。

### 2026-11-25 目标机可达性更新

Rocky 目标 `172.20.44.131:22` 的 TCP 检查通过，ED25519 host key 与既有指纹记录匹配；当前执行环境的 SSH key 未被 `tgmtest` 接受，只读盘点脚本未上传，目标机单实例验收保持 `not-run`。WP-8 分项授权记录中的生产只读检查仍为 `not-authorized`。本更新不改变该授权状态。

### 2026-11-26 目标机可达性更新

对 `172.20.44.131:22` 连续三次 TCP 连接尝试均超时，目标机当前不可达；
未进行 SSH 认证、未上传任何材料。按 §2.2 第 5 项与 §四 判据，Rocky
目标机单实例复验保持 `not-run`，不以本地或 CI 的 systemd 测试代替。
该子项恢复执行的前置条件：目标机网络可达、SSH 认证通过、精确候选包
与授权范围重新核对。

## 五、停止条件

- 需要改变 RLS、GRANT、数据库角色、迁移、audit SQL 或 CI 安全门禁。
- 无法把 metrics listener 限定在获批网络，或 scrape 地址可能通过公网暴露。
- 指标需要未经批准的跨租户明文访问或高基数/敏感标签。
- systemd 与 direct-process 场景的通过判据未获维护人确认。
- 测试要求连接生产数据库、触发生产告警、发 Telegram 消息、启用 timer 或删除真实租户。

## 六、实施记录（2026-11-26）

批准后按 §二 范围实施，未触发 §五 任何停止条件：

- **依赖**：`metrics = 0.24.6`、`metrics-exporter-prometheus = 0.18.3`（`eng/00` §2.3 钉住的版本）。导出器关默认 features：不用它自带的 hyper http-listener / push-gateway，`/metrics` 由本仓 axum 在私有监听上自渲染，少引一整条 hyper 服务端依赖。
- **指标实现**（`crates/app/src/metrics.rs`）：三个核心指标全部从库取（`ops/01` §3.1），经 `app_user` 既有权限 —— `delivery_tasks` 逐租户经 `tenant_tx`（RLS 内）、`tenants` 经 `list_all_active_tenants`、`canonical_events` 是丙类无 RLS。零新增 GRANT / 角色 / 迁移。
- **监听边界**：`TGM_METRICS_LISTEN_ADDR` 是 control-plane 独有的**可选**项；不设置就不暴露指标（本片不启用生产抓取）。设置则必须回环：配置预检（`native_config` 回环断言）与启动期绑定两处都拦，非回环拒绝启动。采集在独立任务里，失败只记脱敏日志并翻转 `tgm_metrics_collect_success`，不阻塞业务、不改投递语义。
- **冻结清单**：`tgm_ingest_latency_seconds`（histogram，秒，无 label）、`tgm_delivery_dead_ratio`（gauge，label 仅 `tenant_id`）、`tgm_delivery_queue_depth`（gauge，无 label）、`tgm_metrics_collect_success`（gauge）。租户级序列基数上限 `max_tenants=1000`，超限截断并告警；内容数据（正文/关键词/凭据/源链接）不进 label 与 exposition。
- **单实例验收**（`bins/tgm/tests/single_instance.rs`，三个场景分开记录，不混为一个判据）：
  1. 同一 user manager 对同名 unit 连续 start 两次 → 第二次 no-op、MainPID 唯一（本机有 `systemd --user` manager，实测通过；无 manager 环境记为未运行，不冒充通过）。
  2. unit 外两个直接进程竞争同一事件 → `processed_at` 行锁 + `idem_key` 去重保证恰好一条投递任务，两进程都存活且可优雅停止。
  3. 真实 `mtproto-worker` 进程在有效租约前拿不到身份（LeaseBusy）；租约过期后接管、fencing token 递增；僵尸的陈旧 token 写不进事件。
- **判据与反证**：`crates/app/tests/metrics.rs` 覆盖受限角色、水位线不回溯、窗口与分母为零口径、租户消失不出序列、截断上限、exposition 白名单（含 `le`/`quantile` 格式标签的口径说明）与非回环绑定被拒。
- **未覆盖项**（按提案保留）：Rocky 目标机同名 unit 复验需单独授权（`not-run`）；X1–X5 多实例混沌按单实例决策保持 `deferred/not-run`；生产抓取与告警渠道需各自运维授权。

## 七、合并后指标测试回归（2026-11-26）

`cloud/dev` 合并提交 `9ce4f666de99378f401c9455376275e8617e8761` 的 CI
run `45766445471` 中，`database` job 的
`sample_reads_only_through_restricted_role_and_window_semantics` 失败。
测试把窗口内 `completed_at` 写成固定的 `2026-11-26T04:00:00Z`，而采集查询
按数据库当前时间减 `failure_window=3600` 秒计算窗口。该测试因此会随运行时间
越过窗口边界后失败。

修复将 `seed_task` 改为通过 `clock_timestamp() - make_interval(...)` 相对
数据库时钟生成时间，窗口内样本使用 60 秒年龄，窗口外样本使用 7,200 秒年龄。
该修复随 PR #146 合入 `cloud/dev`，合并提交为
`7cb8e960be60be61105e499f74735650e6c9f945`；合并后 CI `45794935024`
的 `static` 和 `database` 均通过。

本地验证：`cargo check -p tgm-app --tests` 通过；定向 SQLx 用例通过；
`cargo test -p tgm-app --test metrics -- --nocapture` 的 5 项均通过；
`scripts/ai-driver/ai-gates.sh --static-only` 通过。完整门禁运行到
`offline-build` 时停止，根分区可用空间降至约 6.9 GB；PR #146 CI
完成完整门禁裁决。

## 八、合并后状态更新（2026-11-26）

PR #146 已以 squash 方式合入 `dev`，提交为
`7cb8e960be60be61105e499f74735650e6c9f945`。合并后 CI `45794935024`
的 `static` 和 `database` 均成功，修复后的指标窗口测试已通过。
同一 PR 的 Docker build 与非生产合成验收 job 也成功。

代码实现与 CI 缺陷已收口，G5 总体出口仍为 `not-ready`。Rocky 目标机单实例
复验仍为 `not-run`；生产指标抓取、告警回执、容量压测和运行容量实测仍缺
可核验结果。X1–X5 继续按单实例部署决策延期。

```text
g5_implementation=merged-PR-143
fixed_window_test=merged-PR-146
post_merge_ci=45794935024:success
rocky_single_instance=not-run
production_metrics_and_alerts=evidence-pending
capacity_acceptance=not-run
G5=not-ready
```

```text
[report]
task: G5-runtime-metrics-and-single-instance-acceptance
path: ok
gates: pass
escalation: -
```

## 九、精确候选目标机验收（2026-11-27）

`cloud/dev@0107ba04500e2abf1f7c9a4fd05e9dab5eafaba8` 对应的候选 `candidate-45935022008-a1-0107ba04500e` 已在 Rocky 9.8 目标 `172.20.44.131:22` 完成隔离运行时验收。签名、provenance、二进制、原生预检、六角色运行时和 journal 脱敏均通过。

临时 `rule` user unit 连续两次 start 后 PID 均为 `28507`，满足 §2.2 第 5 项。该结果只覆盖同一 user manager 的同名 unit，不扩大为对 unit 外任意进程的单实例保证。临时 unit、Compose 项目及本轮候选安装副本均已清理，候选材料保留在目标机 `/home/tgmtest/tgm-candidate.45935022008`。

CentOS 7.9 目标 `172.20.44.132:22` 对同一候选的签名、provenance 和二进制检查通过；systemd 219 缺少 `+SECCOMP`，按负向判据拒绝原生预检，未安装候选。该结果满足维护人确认的预期拒绝判据，不改变 CentOS 7 不支持原生服务的结论。

本次没有连接生产数据库、启用生产服务或 timer、清理真实租户、触发 Telegram 外呼。生产指标抓取、告警回执、容量和运行资源实测仍无证据，G5 继续保持 `not-ready`。完整候选来源、摘要和两台目标机边界见 `docs/plan/evidence/G3-G5当前出口复核-2026-11-24.md` §九。

```text
candidate=candidate-45935022008-a1-0107ba04500e
rocky_single_instance=pass
centos7_expected_reject=pass
production_metrics_and_alerts=evidence-pending
capacity_acceptance=not-run
G5=not-ready
```

```text
[report]
task: G5-runtime-metrics-and-single-instance-acceptance-target-refresh
path: ok
gates: candidate-target-acceptance
escalation: -
```
