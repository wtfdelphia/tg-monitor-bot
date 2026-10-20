# WP-5-delivery-media 片 1：媒体投递真机 spike（2026-10-20）

提案：`docs/plan/proposals/WP-5-delivery-media.md`（片 1 于 2026-10-20 批准）。
工具：`crates/tg/tests/media_spike.rs`（`#[ignore]`，不进生产代码）。
环境：平台 `api_id`；测试 Bot 1（测试频道 `@tg_monitor_demo` 管理员，可发帖）；
测试 Bot 2（不是该频道成员）；grammers 0.10.0。凭据只经 `~/.config/tgm-live.env` 传入，
输出已按值扫描，不含 Token 与 `api_hash`。

```bash
set -a; . ~/.config/tgm-live.env; set +a
TGM_LIVE_TARGET_USERNAME=tg_monitor_demo TGM_SPIKE_MEDIA_MB=50 \
  SQLX_OFFLINE=true cargo test -p tgm-tg --test media_spike -- --ignored --nocapture
```

## 一、原始输出

50 MB（pro 档单文件上限）：

```text
[M1] size=50MB upload=45.796445624s message_id=183 same_random_id_resend=ok message_id=Some(183) rss_before=5652KiB hwm_after_upload=16704KiB
[M2] caption_1100_chars=400 MEDIA_CAPTION_TOO_LONG caption_1024_chars=ok
[M3] downloaded=52428800B chunks=100 took=50.820876925s byte_equal=true hwm_after_download=17736KiB
[M4] forward=ok message_id=Some(185) same_random_id_forward=ok message_id=Some(185)
[M5] resolve_peer=400 CHANNEL_INVALID read_source=400 CHANNEL_INVALID forward=400 CHANNEL_INVALID
test result: ok. 1 passed; 0 failed; finished in 112.34s
```

5 MB：

```text
[M1] size=5MB upload=9.052380867s message_id=180 same_random_id_resend=ok message_id=Some(180) rss_before=5068KiB hwm_after_upload=11420KiB
[M2] caption_1100_chars=400 MEDIA_CAPTION_TOO_LONG caption_1024_chars=ok
[M3] downloaded=5242880B chunks=10 took=4.968594072s byte_equal=true hwm_after_download=12088KiB
[M4] forward=ok message_id=Some(182) same_random_id_forward=ok message_id=Some(182)
[M5] resolve_peer=400 CHANNEL_INVALID read_source=400 CHANNEL_INVALID forward=400 CHANNEL_INVALID
```

1 MB，补 M5b（经用户名解析拿真实 `access_hash`）：

```text
[M1] size=1MB upload=1.78903349s message_id=186 same_random_id_resend=ok message_id=Some(186) rss_before=5472KiB hwm_after_upload=10464KiB
[M2] caption_1100_chars=400 MEDIA_CAPTION_TOO_LONG caption_1024_chars=ok
[M3] downloaded=1048576B chunks=2 took=1.178014785s byte_equal=true hwm_after_download=12012KiB
[M4] forward=ok message_id=Some(188) same_random_id_forward=ok message_id=Some(188)
[M5] resolve_peer=400 CHANNEL_INVALID read_source=400 CHANNEL_INVALID forward=400 CHANNEL_INVALID
[M5b] resolve_username=ok read_source=ok found=1 forward_into_source=403 CHAT_WRITE_FORBIDDEN
```

三次运行在测试频道里留下 spike 消息 180–188（文件名 `tgm-spike-*.bin`），可人工删除。

## 二、结论

| 项 | 结果 | 对片 2 的含义 |
|---|---|---|
| M1 Bot 经 MTProto 上传 + `sendMedia` | 50 MB 成功，约 46 s；同一 `random_id` 重发返回同一 `message_id`（183） | 发送侧可行；`random_id` 对 `sendMedia` 去重成立，R11 幂等口径可沿用 |
| M2 caption 超长 | 1100 字报 `400 MEDIA_CAPTION_TOO_LONG`；正好 1024 字成功 | 服务器按字符计 1024，与 `eng/02` 的 `caption_length_max` 一致；超长拆两条（已批准）在发送前判定，不靠服务器报错 |
| M3 流式下载 | 50 MB 分 100 片，约 51 s，逐字节一致 | `iter_download` 分片流可直接接 S3 multipart |
| 内存 | 50 MB 上传 + 下载全程 `VmHWM` 17.7 MiB（起点 5.5 MiB） | 上传和下载都不随文件大小线性增长，方向符合「500 MB 文件内存 < 100 MB」；500 MB 本身没有实测 |
| M4 同频道 Bot FORWARD 媒体 | 成功；同一 `random_id` 重发返回同一 `message_id`（185） | FORWARD 快路径对媒体可用，幂等成立 |
| M5 非成员 Bot，ambient 引用 | 解析、读取、FORWARD 都是 `400 CHANNEL_INVALID` | 用 Bot API chat id 拼出的引用没有 `access_hash`，非成员 Bot 不能这样用 |
| M5b 非成员 Bot，用户名解析 | 能读到公开频道的源消息；FORWARD 回源频道报 `403 CHAT_WRITE_FORBIDDEN`（无写权限） | 公开源频道可以用用户名解析拿 `access_hash`；「从公开源 FORWARD 到 Bot 有权限的另一个目标」没有测，需要第二个目标频道 |

对 spec/02 §4.2「同身份快路径」：

- **私有源频道**：只有发送 Bot 本身就是源频道成员时，FORWARD 才可行。pro 档的源频道由
  MTProto 采集账号订阅，发送 Bot 通常不是成员，这时只能走中转。§4.2「中转是常态路径」
  的判断成立。
- **公开源频道**：非成员 Bot 能读源消息（M5b），FORWARD 到它自己有权限的目标大概率可行，
  但这一跳还没有真机证据，片 2 不依赖它。
- 「事实问题 2」中 Bot API `file_id` 能否给 MTProto 复用，本片没有测。依据已批准的 free 档口径
  （只发文字，或在 Bot 本身就在源频道时 FORWARD），FORWARD 只需要消息 id，不需要 `file_id`，
  这个问题已经不在 free 档的关键路径上。

## 三、没有覆盖的

- 500 MB 文件（超过 pro 档上限，不在首发范围）。
- MTProto **用户账号**作为下载方（本片下载方是 Bot 1）。片 2 的下载在采集侧，由用户账号执行，
  `file_reference` 过期后的重取要在片 2 实测。
- 非成员 Bot 从公开源 FORWARD 到第二个目标频道。
- 媒体组（片 3）。
