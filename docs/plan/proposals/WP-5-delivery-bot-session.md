# WP-5-delivery-bot-session：投递 Bot 的 MTProto session 是否落库

## 任务与判据

任务 id：`WP-5-delivery-bot-session`。由 `WP-5-delivery-sender-channel` 的常驻长跑引出。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G4、§4.3 WP-5（后台任务在进程重启后继续）；
`docs/design/ops/00-部署与运行时.md` §六（停旧起新）；ADR-0033 代价节「每次进程重启都要重新
`bot_sign_in`……重启风暴下的限流没有测过」。

按 `AGENTS.md` §五第 1、3、4 条（新增表或列、凭据处理、修订已定案 ADR），本任务升级，不直接实现。

## 发现

长跑实测（证据 `docs/plan/evidence/WP-5-delivery常驻长跑-2026-10-19.md`）：同一个 Bot 在几分钟内
重新 `bot_sign_in` 到第 4 次左右，`auth.importBotAuthorization` 开始返回 `FLOOD_WAIT`，
服务器给出的等待在 66–305 秒之间，连续触发时等待变长。限流期间该 Bot 的投递停住。

ADR-0033 定案时的依据是「连续 5 次登录未触发限流」，样本只有 3 分钟内的 5 次，所以没有暴露。
现在 delivery 每次重启都从空 session 登录，等于每次重启都消耗一次登录配额。正常部署（停旧起新）
一天几次不会撞上；崩溃循环、频繁发布或运维反复重启时会。

本次已修：限流时等满服务器给的秒数再重试，等待期间响应 SIGTERM（不再是 30 秒冷却后立即重撞）。
这让限流不会自我加剧，但不缩短停顿。

## 选项

- **A 维持不落库**（现状）。重启频繁时投递停几分钟，写进 ADR-0033 代价节即可，零 schema 改动。
- **B Bot session 加密落库，重启复用。** `identity_secrets` 现在一行一份密文：Bot 行存 Token，
  MTProto 行存 session。Bot 要同时有 Token 与 session，需要新增列（如 `session_ciphertext`、
  `session_nonce`，同一 DEK 加密）或新表。复用 session 后重启不再调 `auth.importBotAuthorization`，
  限流问题消失；代价是多一份凭据密文（session 等价于登录态，泄露面与 Token 相当，
  Token 被重置时 session 一并失效）、一份迁移、一份修订 ADR-0033 的新 ADR。
- **C 进程内退避上限 + 告警，不落库。** 只在运维侧发现「频繁重启」，不改 schema。
  不解决停顿，只让它可见。

## 建议

先 A（本批已提交），观察生产上重启频率；若首发前的部署方式会频繁重启 delivery（例如滚动发布、
自动扩缩），再走 B。B 落地时要一起解决：session 写入时机（登录成功后、退出前）、Token 轮换后
旧 session 作废、与 `mark_bot_sender_unauthorized` 的关系。

## 需要批准

- 是否接受 A 作为首发形态。ADR-0033 已定案、正文不改；若要把限流实测写进决策记录，需要一份新 ADR。
- 是否立项 B（新增列/表 + 新 ADR）。

## 批准记录（2026-10-20）

测试环境提供方批准方案 B（加密落库）。落地：迁移 `migrations/0021_identity_secret_bot_session.sql`、
`docs/design/decisions/ADR-0034-投递Bot的session加密落库.md`。三个待解问题的处理：

- 写入时机：Token 登录成功后立即写回，不等退出（SIGKILL 不经过退出路径）。
- Token 轮换：control-plane 覆盖凭据的同一条 UPSERT 清空 session；delivery 写回时校验 DEK 密文
  仍是读出时那一份，Token 在登录期间被换掉就丢弃。
- 与 `mark_bot_sender_unauthorized` 的关系：复用 session 时 auth key 被拒只清 session、退回 Token 登录，
  不标 `unauthorized`；Token 本身被拒才标。
