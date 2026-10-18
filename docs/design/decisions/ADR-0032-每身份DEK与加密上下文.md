# ADR-0032：DEK 改为每身份一个，KEK 调用绑定加密上下文

状态：已定案（2026-10-18）

## 决定

- 每个身份各用一个 DEK，不再是每租户一个。每次写入凭据都在本地新生成 DEK，
  用它加密凭据，再用 KEK 包裹，DEK 密文与 `kek_id` 和凭据密文存在同一行
  （`identity_secrets`）。`credential_keys` 停止写入，表保留。
- `control-plane` 持有 KEK 配置，但只调用 `wrap`。生产上它的 KMS 权限只有
  `kms:Encrypt`；`kms:Decrypt` 只授予 `bot-gateway`、`mtproto-worker`、`delivery`。
  `control-plane` 启动时做 encrypt-only 自测：wrap 必须成功，unwrap 必须被拒。
- 每次 wrap/unwrap 都带加密上下文 `{tgm:tenant_id, tgm:identity_id}`。
  `KmsKek` 作为 KMS `EncryptionContext` 发出，`EnvKek` 用 AAD 实现相同语义。
  `Kek` trait 的 `wrap`/`unwrap` 因此多一个上下文参数。

本条修订 [[ADR-0011]] 中「每租户一个 DEK」与「`control-plane` 拿不到 KEK 相关配置」两处。
信封加密本身、`kek_id` 是列、`EnvKek` 在生产拒绝启动，这些都不变。

## 背景

规格要求 Bot Token 只能经 Web 提交，端点挂在 `control-plane` 上；另一方面
[[ADR-0011]] 要求 `control-plane` 不具备解密能力。每租户共用一个 DEK 时，
往已有 DEK 下写入新凭据必须先 unwrap 它，也就是调用 KMS `Decrypt` ——
两条要求在这里直接冲突，规格里没有讨论过。提案
`docs/plan/proposals/P1-BOT-TOKEN-INTAKE.md` 比较了三个选项：

- 放宽：`control-plane` 同时拿 Encrypt 和 Decrypt。最简单，但放弃了「控制面解不开」这条保证。
- 公钥收件箱：控制面用公钥加密，持解密权的组件后台转封。守住保证，但要多一套密钥对、
  一个后台任务，录入到可用之间有延迟。
- 每身份 DEK（本决定）：写入只需要 `kms:Encrypt`，读取才需要 `kms:Decrypt`。

选第三个，理由是「控制面解不开」可以由 IAM 证明，并在每次启动时实测，
而不是写在部署模板里等人遵守。原来那条保证靠「配置缺失」成立，
现在靠 KMS 权限成立，后者能产生 AccessDenied 这个可观测的反证。

加密上下文是同一轮决定的。没有它时，能写库的人可以把身份 A 的 DEK 密文
抄给身份 B，KMS 照样解开；凭据密文的 AAD 已经绑定身份，但 DEK 这一层没有。
加上之后，搬运在 KMS 层就失败；CloudTrail 每条 Decrypt 都带租户与身份，
可以按身份审计；IAM 也可以按上下文加条件。

## 代价

- `Kek` trait 签名变了（eng/01 的契约），所有实现与调用方都要传上下文。
  上下文键名是线上契约，改名会让已有密文解不开。
- `control-plane` 的进程环境里多了 KEK 配置。`EnvKek` 是对称的，本地开发时
  控制面其实解得开 —— 这与 [[ADR-0012]] 已接受的本地差距同一性质，
  「控制面解不开」只在 `KmsKek` + IAM 下成立。
- 每次写入都多一次 KMS `Encrypt`（原先同租户第二个身份起可以复用 DEK）。
  读取仍是每次一次 `Decrypt`，量级不变。
- 覆盖写入会换新 DEK。写入方解不开旧 DEK，所以没有「沿用旧 DEK」这条路。
- `credential_keys` 成了停写的遗留表；新迁移不能改旧迁移（[[ADR-0021]]），
  所以它留在库里，删除路径仍会清它。
- 新增 `identity_secrets` 两列为 `NOT NULL`、无默认值。迁移以「表里没有旧行」为前提，
  这个前提不成立时迁移会失败，需要人工处理。

## 相关

[[ADR-0011]]、[[ADR-0012]]、[[ADR-0021]]。
提案 `docs/plan/proposals/P1-BOT-TOKEN-INTAKE.md`；落地在 `spec/05-安全与租户隔离.md`、
`spec/03-数据模型.md`、`eng/01-配置与Secret.md`、`ops/02-OCI的KMS凭据接入.md`。
