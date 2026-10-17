# PRE-003：KmsKek 生产后端

## 当前状态

`tgm-core` 已有 `Kek` trait 与开发用 `EnvKek`。生产画像会拒绝 `EnvKek`；
`tgm serve --role ...` 的 `TGM_KEK_BACKEND=kms` 路径现已接入 `KmsKek` 与启动期往返自测。

本地开发继续使用 `EnvKek`，不需要 AWS 账号。PRE-003 只实现 AWS KMS 生产后端；
目标生产平台尚未确定时，真实 KMS 验收保持待定，不阻塞本地开发。

`KmsKek` 不能直接塞进 `tgm-core`：它需要 AWS SDK，且本地与 CI 没有真实 KMS 凭据。
同时，新增 AWS 依赖需要修改根 `Cargo.toml` 与 `Cargo.lock`，这两个文件属于受保护路径。

2026-10-16：获批实施已在最新 `dev` 基线上完成本地实现与门禁；获授权的 `KmsTest` 会话
已完成真实 KMS 往返及项目内 `KmsKek` 启动自测。生产运行角色和目标部署环境尚未验证，
因此本提案不记为生产出口通过；逐项结果见
`docs/plan/evidence/PRE-003-KmsKek验证-2026-10-16.md`。

2026-10-17：信任边界复核补充 KMS endpoint resolver 回归及 STS/IMDS endpoint 环境变量
的预检拒绝项。默认凭据链 provider 解析、profile 内服务 endpoint 和目标 workload IAM
仍未验收；相关结果见上述验证记录与
`docs/plan/proposals/PRE-003-KMS信任边界收口.md`，生产出口继续待定。

## 需要批准的改动

以下条目记录本提案获批时的实施范围；实现与验证结果以本文件状态说明和
`docs/plan/evidence/PRE-003-KmsKek验证-2026-10-16.md` 为准。

### 1. 依赖与落点

- 在允许依赖 AWS SDK 的 crate 中加入固定版本的 `aws-sdk-kms` 与必要的配置加载依赖；
- 保持 `tgm-core::Kek` trait 签名不变；
- 将 raw AWS KMS 调用收敛在单独的生产后端模块，不向 `tgm-app`、`tgm-db` 或 `tgm-tg`
  泄漏 SDK 类型；
- 在根 `Cargo.toml` 与 `Cargo.lock` 中统一依赖版本，并补齐 `cargo deny` 结果。

### 2. 加密行为

- `wrap` 调用 KMS `Encrypt`，明文只传 DEK，不把 KEK 明文拉回进程；
- `unwrap` 调用 KMS `Decrypt`，只在一次调用生命周期内持有 DEK，并沿用现有
  `Secret<[u8; DEK_LEN]>` 清零边界；
- `kek_id` 使用配置的 KMS key ARN；
- KMS SDK 错误统一映射为 `KekError::Crypto`，不得把密钥材料、请求参数或云端错误细节
  返回给客户端日志。

### 3. 配置与启动

- `TGM_KEK_BACKEND=kms` 时要求 `TGM_KMS_KEY_ARN`；
- `backend=kms` 启动必须执行一次 `wrap → unwrap → 比对` 自测；
- 缺少 ARN、权限不足、区域或凭据不可用时启动失败，不降级到 `EnvKek`；
- `backend=env` 的开发行为和 production 拒绝规则保持不变。

## 验证边界

本地无真实 KMS 凭据时，只能完成编译、单元测试、错误映射和 mock HTTP 形状验证；不能
把 mock 通过写成生产 KMS 已验证。生产出口还需要一次真实 KMS key 的往返自测，确认 IAM
权限、key policy、区域配置和失败处置。

## 出口判据

- `KmsKek` 编译并实现 `Kek` trait；
- `backend=kms` 缺少配置或无法访问 KMS 时非零退出；
- 成功路径完成启动期往返自测；
- 生产路径不记录 KEK、DEK、密文或 AWS 凭据；
- 静态、数据库和依赖审计卡口通过；
- 真实 KMS key 的往返记录归档到生产就绪证据，不把 mock 结果替代真实验证。

## 处理边界

本提案不修改迁移、RLS、审计断言、Telegram 真机流程或产品范围；这些事项分别由
`PRE-001`、`PRE-002` 和 `T-0002` 的出口负责。

[report]
task: PRE-003
path: escalated
gates: pass
escalation: docs/plan/proposals/PRE-003-KMS信任边界收口.md
