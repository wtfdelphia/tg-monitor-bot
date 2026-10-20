# ponytail-debt —— 刻意简化的债务台账

`AGENTS.md` §4.4 的台账：任务归档后（PR 合并、backlog `status: done`），
把实现里留下的 `ponytail:` 注释登记到这里。只增不清 —— 简化被升级掉时
把该行的「状态」改成日期与去向，不删行。

引用纪律同 `backlog/README.md`：不复制判据数字，引用唯一出处。

## 台账

| 任务 | 位置 | 天花板 | 升级触发 | 状态 |
|---|---|---|---|---|
| NEXT-001-backend（PR #4） | `crates/core/src/oidc.rs` OIDC 登录 state 存储 | 进程内状态：控制面扩容或重启时不共享 | 会话安全面批准后改为带 TTL 的共享存储 | 未升级（2026-10-20 补登） |
| NEXT-P1（PR #12） | `crates/db/src/credentials.rs` `kek_id` 不符错误 | 不支持多个 KEK 并存 | KEK 轮换落地时，改为按行 `kek_id` 选 KEK | 未升级（2026-10-20 补登） |
| NEXT-P1-mtproto-session（PR #14） | `crates/tg/src/session.rs` 快照序列化 | peer 缓存不设上限，快照随会话见过的 peer 线性增长 | 单份快照超过 1 MiB 时按最近使用裁剪 | 未升级（2026-10-20 补登） |
| WP-5-delivery-media 片 2（PR #25） | `crates/app/src/delivery.rs` `split_text_random_id` | 拆分第二条的派生 `random_id` 与同一 (sender, target) 下其他任务碰撞概率约 2^-63 | 出现「编辑第二条文字」的需求时，改为 `delivery_tasks` 独立列 | 未升级 |
| WP-5-delivery-media 片 2（PR #25） | `crates/app/src/media_relay.rs` `run_relay_once` | 下载完才知道 `content_hash`，做不到 spec/02 §4.2 第 3 条「命中档案跳过下载」，只在下载后去重存储 | 中转流量成为成本瓶颈时，按 document id 预判跳过 | 未升级 |
| WP-5-delivery-media 片 2（PR #25） | `crates/tg/src/client.rs` `send_uploaded_media` | 视频与音频不带时长、尺寸等 document attributes，可能显示为普通文件 | 出现「视频不能内联播放」时，改为在 job 里保存原 attributes | 未升级 |

## 与上游纪律的适配与分叉（只增不清）

代码简化的债务记上表；「本仓纪律与上游规则的分叉」记这里 —— 分叉不是债，
但必须可查。逐条全文与锚点见 `../../scripts/ai-driver/refs/`。

| 日期 | 上游 | 适配/分叉 | 升级触发 |
|---|---|---|---|
| 2026-09-23 | ponytail | 高风险六域以「加密与凭据」替入上游的「模型执行」（上游无此域的本仓对应物） | `plan/00` §6.4 演进候选项含 AI 语义过滤，届时回来重核 `AGENTS.md` §4.1 |
| 2026-09-23 | caveman-commit | subject/body 中文书写（上游要求英文祈使句）；本仓高风险域 body 清单在上游触发条件之外 | 上游改 subject 语言规则时重核 |
