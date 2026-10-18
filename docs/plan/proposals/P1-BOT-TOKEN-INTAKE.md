# P1-BOT-TOKEN-INTAKE：Bot Token 录入与控制面无解密权的冲突

## 发现

`spec/07` §3.3 把 `POST /api/v1/identities/bot` 定为 Web 端点，也就是挂在 `control-plane` 上。另一方面，`spec/05` §七、`spec/02` §3.1、ADR-0011 和 `eng/01` §二 都要求 `control-plane` 不具备解密能力，并且它拿不到 KEK 配置。

#12 合并的 `store_identity_secret` 用的是每租户一个 DEK。往已有 DEK 下写入新凭据，必须先 `unwrap` 这个 DEK，也就是调用 KMS `Decrypt`。所以直接在控制面调用它，就等于给 `control-plane` 开了解密权。这个冲突在规格里没有被讨论过。`spec/05` 的局限节也承认「没有测试能证明 control-plane 真的解不开凭据」。

按 `AGENTS.md` §五第 3、4 条（凭据安全面、需要 ADR），本任务升级，不直接实现。

## 选项

| 选项 | 做法 | 守住无解密权 | 代价 |
|---|---|---|---|
| A 放宽 | `control-plane` 增配 KEK，同时拿 `kms:Encrypt` 和 `kms:Decrypt` | 否 | 推翻 ADR-0011 的核心保证；最简单 |
| B 公钥收件箱 | 控制面用公钥加密 token，写入待处理密文；持解密权的 `bot-gateway` 在后台转封为 DEK 密文 | 是 | 新列或新表（迁移）、后台任务、密钥对管理；录入到可用之间有延迟 |
| C 每身份 DEK | 每个身份各用一个 DEK。控制面本地生成新 DEK（复用 `generate_dek`），先用它加密 token，再用现有 `Kek::wrap` 包裹（`KmsKek` 下即 `kms:Encrypt`），随后清零明文 DEK，只存 DEK 密文。控制面只需 `kms:Encrypt`，不给 `Decrypt` | 是 | DEK 粒度从每租户改为每身份，需要迁移（`identity_secrets` 加 `dek_ciphertext`/`kek_id`，或调整 `credential_keys` 主键）和新 ADR 修订 ADR-0011；#12 的 `ensure_wrapped_dek` 需要改写 |

## 建议

选 C，理由：

- 控制面的 IAM 权限只需要 `kms:Encrypt`，即只能加密新数据，`kms:Decrypt` 只授予三个解密组件。这样「控制面解不开」可以由 IAM 证明：ops/02 的验收清单里可以增加一条反证，用控制面凭据调 `Decrypt` 应返回 AccessDenied。这正好补上 spec/05 局限节里那一环。
- 不引入后台任务，录入后立即可用。
- 删除语义更简单：删除身份时连同它的 DEK 一起删除，不再需要「该租户是否还有其他身份」的判断（spec/04）。
- KMS 调用量不变：每次加载凭据仍然只调一次 `Decrypt`。

`Kek` trait 不用改：控制面只调 `wrap`，解密组件只调 `unwrap`。这条边界依靠 IAM 强制，不依赖代码自律。`EnvKek` 是对称的，本地开发没有这层隔离，这一点与 ADR-0012 已经接受的差距相同。

## 需要批准

1. 选定方案，推荐 C。
2. 新写一份 ADR 修订 ADR-0011 的 DEK 粒度；ADR-0011 本身只增不改。
3. 迁移形态：推荐在 `identity_secrets` 增加 `dek_ciphertext`、`kek_id` 两列，`credential_keys` 停止写入但保留，避免改动已应用的迁移。
4. `control-plane` 角色增配 `TGM_KEK_BACKEND`/`TGM_KMS_KEY_ARN`，预检规则要相应调整；ops/02 的 IAM 模板拆成「加密角色」和「解密角色」两份。

## 决定（2026-10-18）

用户批准 **C + KMS 加密上下文**。在 C 的基础上，每次 wrap/unwrap 带
`{tgm:tenant_id, tgm:identity_id}`：`KmsKek` 发 `EncryptionContext`，`EnvKek` 用 AAD。
这改了 `Kek` trait 的签名。决策见 `docs/design/decisions/ADR-0032-每身份DEK与加密上下文.md`。

落地范围：

- 迁移 `migrations/0020_identity_secret_dek.sql`：`identity_secrets` 加 `dek_ciphertext`、`kek_id` 两列（`NOT NULL`）。
- `crates/core/src/kek.rs`：`KekContext`，以及 control-plane 用的 `encrypt_only_self_test`。
- `crates/db/src/credentials.rs`：写入只 wrap，不 unwrap；新增 `create_bot_identity`，建身份、写密文、记审计在同一个事务里完成。
- `crates/app/src/control.rs`：`POST /api/v1/identities/bot`。只接受 Web 入口，要求 Owner/Admin，token 先在本地校验格式，再调 getMe 核对，响应不回显 token。
- 预检：control-plane 增配 KEK 变量；`backend=kms` 时启动做 encrypt-only 自测。
- 文档：spec/03、spec/04、spec/05、eng/01、ops/02（IAM 拆成加密、解密两套身份）。

证据见 `docs/plan/evidence/P1-BOT-TOKEN-INTAKE-2026-10-18.md`。
