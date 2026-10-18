# P1-BOT-TOKEN-INTAKE 每身份 DEK、加密上下文与 Bot Token 录入

记录日期：2026-10-18

任务 id：`P1-BOT-TOKEN-INTAKE`

判据出处：`docs/design/spec/07-接口契约.md` §3.3（`POST /api/v1/identities/bot`）、
`docs/design/spec/05-安全与租户隔离.md` §七（控制面无解密权）、
`docs/design/decisions/ADR-0032-每身份DEK与加密上下文.md`；
测试分层见 `docs/design/eng/03-测试策略.md` §一。
提案与批准记录：`docs/plan/proposals/P1-BOT-TOKEN-INTAKE.md`（方案 C + 加密上下文）。

## 一、范围

- `crates/core/src/kek.rs`：新增 `KekContext{tenant_id, identity_id}`，`Kek::wrap/unwrap`
  带上下文；`EnvKek` 把上下文编码为 AAD；新增 `encrypt_only_self_test`
  （wrap 必须成功、unwrap 必须失败）。
- `bins/tgm/src/kms_kek.rs`：Encrypt/Decrypt 请求带 `EncryptionContext`
  （键名 `tgm:tenant_id`、`tgm:identity_id`）。
- `migrations/0020_identity_secret_dek.sql`：`identity_secrets` 增加
  `dek_ciphertext`、`kek_id` 两列，`ADD COLUMN IF NOT EXISTS`；`credential_keys` 停写保留。
- `crates/db/src/credentials.rs`：写入只 wrap 不 unwrap；新增 `create_bot_identity`，
  身份、密文、审计 `identity.create` 同一事务，事务级 advisory lock 防并发重复注册。
- `crates/app/src/control.rs`：`POST /api/v1/identities/bot`。只接受 Web 来源、
  要求 Owner/Admin、本地校验 token 格式后调 getMe 核对 `is_bot` 与 id；
  未配置 KEK 时 fail closed；响应与日志不回显 token。
- 预检：control-plane 必填 `TGM_KEK_BACKEND`；`kms` 下启动跑 encrypt-only 自测，
  `env` 下只验 wrap；三个解密角色仍跑完整 `self_test`。
- 文档：ADR-0032（修订 ADR-0011 的 DEK 粒度，ADR-0011 只追加指向），
  spec/03、spec/04、spec/05、eng/01、ops/02（IAM 拆为加密、解密两套身份，
  加 `kms:EncryptionContextKeys` 条件与 Decrypt 被拒的验收项）。`openapi.json` 重新生成。

## 二、验证记录

- 完整本地门禁 `scripts/ai-driver/ai-gates.sh` rc=0：fmt、clippy、offline-build、deny、
  newtype、openapi-check、driver-regressions、migrate、sqlx-check、migrate-idempotent
  （19 个文件含 0020，两遍逐字节相同）、test、audit-rls、double-pass
  （每条断言被对应破坏单独打红，收尾目录快照与第一遍相同）全部通过。
- 本地库 `_sqlx_migrations` 最大版本 20，`identity_secrets` 已含两列新列。
- `crates/db/tests/credentials.rs` 8 项以受限角色连接：密文落库与往返、
  encrypt-only KEK 下写入成功、跨租户读写被拒、整行复制到另一身份不能解密、
  DEK 换上下文不能 unwrap、他 KEK 包裹的 DEK 被拒、`create_bot_identity`
  原子性与审计、同一 bot 并发注册只产生一个身份。
- `crates/app/tests/control.rs` 4 项 `bot_identity_intake_*`：getMe 核对且不回显 token；
  Bot 来源、只读角色、格式错误在任何外部调用前被拒；getMe 不一致被拒；无 KEK 时 fail closed。
- `kek.rs` 单元测试覆盖 AAD 绑定与 `encrypt_only_self_test` 正反两面；
  `kms_kek.rs` mock 测试断言请求体带 `EncryptionContext`。
- 门禁后只改写了 credentials 的一个测试，随后 fmt、clippy、credentials 8 项、
  bot_identity 4 项重跑全绿。

## 三、反证

反证改动只在本地临时做，均已还原：

1. `EnvKek` AAD 置空：上下文绑定相关测试报红。
2. 写入路径加一次 unwrap：encrypt-only 相关两项报红。
3. 去掉端点的 Web-only 判断：对应拒绝测试报红。

第一版「DEK 搬到另一身份」测试连同 nonce/密文整行复制，失败原因可能是
DEK 与密文不配而不是上下文，测不到上下文绑定；已改写为
`stored_dek_does_not_unwrap_under_another_context`，直接以另一上下文 unwrap 同一 DEK。

## 四、未验证

- 真实 IAM 下 control-plane 凭据调 `Decrypt` 返回 AccessDenied，
  以及 `kms:EncryptionContextKeys` 条件生效（按 ops/02 验收清单在授权环境执行）。
- 真实 Telegram getMe（本轮只用 mock）。
- 「control-plane 解不开凭据」目前靠启动期 encrypt-only 自测与 IAM 拆分保证，
  尚未作为反向测试编号登记进 `eng/03` §三（登记需改基线清单，另行提案）。
- MTProto session 序列化落库尚未接入，计划复用 `store_identity_secret`。
