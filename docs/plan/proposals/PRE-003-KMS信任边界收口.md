# PRE-003-KMS 信任边界收口

## 发现

PRE-003 的 `KmsKek::from_arn` 从 key ARN 提取区域，再把 `aws_config::load_defaults` 得到的共享配置交给 `aws_sdk_kms::config::Builder::from`。因此凭据提供者和 KMS endpoint 的最终选择不只由 `TGM_KMS_KEY_ARN` 决定。

配置预检和角色白名单已登记多种 AWS 凭据相关环境变量，但该校验只判断变量名是否允许，不限制 SDK 最终选择的 credential provider。代码目前调用固定版本 `aws-config` 的默认凭据链；profile、web identity、ECS/container 和 EC2 IMDS provider 的解析路径尚无本项目验收记录。`sso` 与 `credentials-process` features 未启用，但不能据此推断其余 provider 已被限定为目标部署实际使用的来源。

KMS `Encrypt` 请求携带 DEK。生产 KMS client 已使用由 ARN 区域解析的 endpoint resolver；新增 mock 回归捕获了最终 KMS URI，并覆盖用 fixture 注入的全局、服务级及 profile endpoint 值。该测试未读取真实环境变量或 profile 文件，也不覆盖默认凭据链内部的 STS、container 或 IMDS 请求。复核还发现预检清单未登记 `AWS_ENDPOINT_URL_STS`、`AWS_EC2_METADATA_SERVICE_ENDPOINT` 与 `AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE`；这些次级请求的 endpoint 边界与运行时行为仍待验证。

## 建议范围

### 1. 凭据来源边界

- 对齐固定版本 `aws-config` 的默认凭据链行为与本仓 `eng/01` 的按角色配置白名单。
- 明确 KMS 工作角色允许的凭据来源：静态短期环境凭据、AWS profile、web identity、容器凭据、实例身份或其组合；只登记实际部署会使用且已确认的来源。
- 保持不需 KMS 的角色拒绝已知 AWS 凭据配置；不得把未知变量静默通过误当成来源已经受支持。
- 覆盖凭据缺失、来源不可用与来源冲突时拒绝启动且不降级的行为。

### 2. KMS 请求目标边界

- 核对固定版本 SDK 的全局及 KMS 专用 endpoint 环境变量、共享配置文件 endpoint 项和 endpoint resolver 优先级。
- 让生产 `KmsKek` 的 endpoint 由 ARN 中的 region 与 AWS partition 规则决定；外部 endpoint 覆盖不得将 `Encrypt`/`Decrypt` 重定向至非批准服务。
- 如保留测试自定义 endpoint，仅允许通过测试专用构造路径注入；生产构造函数不得继承该入口。
- 不在日志中输出凭据、DEK、密文或 endpoint 中可能包含的敏感信息。

### 3. 验收

- 用固定 SDK 版本的测试覆盖每种获准凭据来源的解析路径，并验证未批准来源在相应角色上被拒绝或明确不受支持。
- 用 mock HTTP 服务验证生产配置在注入全局、服务专用及 profile endpoint 覆盖时，KMS 请求仍只能发往 ARN 对应的区域 endpoint；测试专用注入另行验证。
- 验证 `control-plane` 不获取 KMS 凭据配置，KMS 工作角色配置最小化，凭据无法取得时启动非零退出。
- 更新 `docs/design/eng/01-配置与Secret.md` 与 PRE-003 验证证据，区分已测试的 credential provider、endpoint 解析测试与真实 workload IAM 验收。
- 通过 `cargo fmt`、定向配置与 KMS 测试及完整 `scripts/ai-driver/ai-gates.sh`；本地门禁不能代替远端 PR CI 或目标 workload role 验收。

## 批准边界

本提案触及 `crates/core/src/config.rs` 的角色配置白名单及 `bins/tgm/src/kms_kek.rs` 的 AWS 客户端构造，属于仓库 `AGENTS.md` §五所列安全面。获批前不修改实现、AWS SDK 配置、配置事实源或 CI，不创建 backlog 任务，也不据此宣称 PRE-003 的生产出口通过。

## 2026-10-17 复核状态

本提案已获批范围内的 KMS endpoint resolver 回归和角色配置白名单已有代码与测试；KMS 定向测试 5 项、配置测试 20 项、`EnvKek` 测试 12 项通过，完整本地 `scripts/ai-driver/ai-gates.sh` 结果见验证记录。上述结果不覆盖各 credential provider 的真实解析路径，也不证明 STS、container 或 IMDS 次级请求的 endpoint 边界。

本次按已批准范围补充 KMS endpoint resolver 回归、拒绝 STS/IMDS endpoint 环境变量及对应角色配置反例测试，并更新配置事实源与验证记录。profile 文件内的次级服务 endpoint、各 credential provider 的请求解析和目标 workload IAM 仍未验收；这些未完成项保持显式，不据此宣称生产就绪。

[report]
task: PRE-003-KMS-TRUST-BOUNDARY
path: escalated
gates: pass
escalation: docs/plan/proposals/PRE-003-KMS信任边界收口.md

## 2026-10-17 OCI 凭据来源定案与实施

部署目标为 OCI VPS。过渡阶段用 G：静态 IAM user key，由环境变量注入。目标阶段用 A：IAM Roles Anywhere，SDK 通过 `credential_process` 调用 `aws_signing_helper`。B 方案是刷新 shared credentials 文件，已被反证排除，原因是 SDK 只解析一次文件。探针证据见验证记录。

经批准实施以下改动：

- 根 `Cargo.toml` 中 `aws-config` 开启 `credentials-process` feature，`Cargo.lock` 随之更新。新增回归测试 `profile_credential_process_is_compiled_in`。反证：去掉该 feature 后，这条测试因 SDK 报「requires following cargo feature(s)」而失败。
- 配置预检在静态 key 与另一凭据来源变量并存时报错，避免 Environment provider 静默覆盖 Roles Anywhere。
- 配置预检在 `TGM_KEK_BACKEND=kms` 时要求 `AWS_EC2_METADATA_DISABLED=true`，避免凭据链末端访问 OCI 的 `169.254.169.254`。

仍未验收：真实 `aws_signing_helper` 与 trust anchor/profile/role 的配置；G 阶段 IAM user 策略与来源 IP 条件；真实 KMS 往返；静态 key 轮换。预检只按变量名判定，服务账户 HOME 下默认路径的 `.aws/config` 不在其检查范围内，靠部署约束兜底。
