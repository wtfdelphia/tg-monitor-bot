# WP-5 delivery 片 3 A：相册变体真机验收（2026-10-20）

任务：补齐 `WP-5-delivery-media-group-2026-10-20.md` §五 留下的三种未真测相册：
图片与视频混合、说明超过 1024、单成员。批准范围：`docs/plan/proposals/WP-5-delivery-media.md`
片 3 的批准记录（真机验收属第 8 条）。本次不新增迁移、不改安全面。

## 一、真机工具

`crates/app/tests/media_relay_live.rs`：原三图用例的流程抽成 `relay_album`
（Bot 发源相册 → 用户身份读取并规范化 → 窗口内不扇出 → 聚合、规则只生成一个任务 →
成员中转进 MinIO → Bot 整组投递），新增三个 `#[ignore]` 用例：

```text
live_mixed_media_group_relay          图片 + 视频 + 图片
live_long_caption_media_group_relay   说明 1122 个 UTF-16 码元，拆成「相册 + 一条文字」
live_single_member_media_group_relay  单成员 sendMultiMedia 探测 + 只采到一个成员的整组
```

新增环境变量 `TGM_LIVE_ALBUM_VIDEO_FILE`：仓库外 `~/.config/tgm-qr/album-fixture.mp4`，
本机 cv2 生成的 160×96、20 帧、无音轨 MPEG-4 Part 2 视频，4829 字节，权限 0600。
本机没有 ffmpeg 与 H.264 编码器，未测 H.264 来源。

## 二、混合相册暴露的平台差异与修复

首次运行时，Bot 发送源相册就失败：`MEDIA_EMPTY`，调用点 `messages.sendMultiMedia`。
用临时探针（已删除，不进提交）逐项排除：

```text
单发视频（只带 Filename）           → 成功，用户读回 media_type=ANIMATION
两个 DOCUMENT 组相册                → 成功
两个视频组相册（Filename + Video）  → MEDIA_EMPTY
上一行再加 nosound_video            → 成功；单发视频读回 media_type=VIDEO
```

根因：无音轨的 mp4 不带 `nosound_video` 时，平台把它转成 GIF（ANIMATION），
GIF 不能进相册。修复在共享上传入口 `upload_input_media`：VIDEO 带 `DocumentAttributeVideo`
（时长与宽高填 0，`supports_streaming`），并置 `nosound_video`。单条中转 `send_uploaded_media`
与相册 `prepare_album_media` 走同一入口，一处修复两处生效。
L1：`crates/tg/src/client.rs` 的 `only_videos_upload_with_a_video_attribute`，
只有 VIDEO 带视频属性，DOCUMENT 与 AUDIO 不带，文件名属性始终在首位。

`nosound_video` 对有音轨的视频是否会去掉声音，本次没有有音轨的夹具，未验证。
这一项与「时长、宽高填 0」一起记在 `ponytail:` 注释里，PR 合并后登记台账。

## 三、真机结果

测试频道 `@tg_monitor_demo`，源与目标同一频道，发送 Bot `@tg_monitor_1_bot`，
采集身份为扫码登录的测试用户（home DC 5）。先跑 `live_user_session_resume` 缓存频道
access_hash，ambient 读取的 `CHANNEL_INVALID` 与前次记录一致。

```text
[M] source_ids=[229, 230, 231] source_group=14340094624365669 first_target=232 target_group=Some(14340094697929989) types=[Some("PHOTO"), Some("VIDEO"), Some("PHOTO")] same_random_id=232
[L] source_ids=[216, 217] first_target=218 album=[218, 219] text_id=220 utf16=1122 same_random_id=218 text_again=220
[S] probe=[212] probe_grouped=None source_ids=[213, 214] source_group=14340092510629669 captured=1 first_target=215 target_grouped=None same_random_id=215
```

- 混合：目标是新相册，成员类型与顺序保持 PHOTO / VIDEO / PHOTO，说明在首成员；
  相同 random_id 重发返回原消息。
- 超长说明：Bot 发出的说明受 1024 上限约束，测试把首成员 canonical event 的说明换成
  1122 码元的长文，模拟 Premium 来源。目标相册两个成员都不带说明，紧跟一条全文；
  相册与文字（派生 `split_text_random_id`）各自重发，都返回原消息 id。
- 单成员：一个成员的 `sendMultiMedia` 平台接受，但读回 `grouped_id=None`，
  所以来源不会出现「单成员相册」。单成员整组只会来自聚合时只采到一个成员；
  用两成员源相册只写首成员复现，整组按 `deliver_media` 的单条 sendMedia 发出，
  目标无 grouped_id，说明完整，重发去重。

重构后三图用例回归通过（source 235–237，target 238，FORWARD 241–243）。

频道里留下的额外消息：208–211 是单成员用例一版截断工具的运行产物，
221–228 是临时探针，不是中转重试产生的重复消息。

## 四、门禁

完整 `scripts/ai-driver/ai-gates.sh` rc=0，输出「已执行的全部本地卡口通过」。
测试合计 338 passed / 0 failed / 15 ignored：比 PR #30 基线多 1 条 L1（视频属性），
多 3 条 ignored（新增真机用例）。`audit-rls` 为 18 条断言 0 条报红，`double-pass` 通过，
迁移幂等 22 个文件重跑后库状态逐字节相同。本地 `cargo-deny` 为 0.20.2，CI 钉 0.19.1，
本地结果不能替代 CI 裁决；改动尚未提交。

## 五、边界

本次只覆盖无音轨 MPEG-4 Part 2 视频；有音轨与 H.264 来源、音频成员相册、
文档成员与图片混排（平台本就不允许）均未测。扫码仍是测试工具，不宣布 WP-2 的 QR 出口判据通过。
