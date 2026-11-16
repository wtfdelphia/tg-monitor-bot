# G1：OIDC 真实身份映射与联调

任务：`G1-oidc-provider-alignment-20261116`。

状态：已批准实施。用户于 2026-11-16 在本任务对话中明确批准本提案。认证身份映射修复、
算法/profile 配置约束和相关反向测试按下列范围实施；真实联调仍需受控配置到位。
本提案与本片其他材料一起保留，按用户要求不单独提交或开 PR。

## 一、问题与依据

基线：`cloud/dev@82f79339922585c04621b15953e119704ebf3292`。

当前 `crates/app/src/control.rs::validate_oidc_id_token` 把 JWT `sub` 交给
`crates/core/src/oidc.rs::validate_id_token_claims` 解析为 Telegram 用户 ID，再用于成员反查
和 Web 会话签发。Telegram 官方当前文档把 OIDC subject `sub` 与 `profile` scope 返回的
Telegram 用户字段 `id` 分开定义；文档示例两者不同。当前桩测试将两者混同，不能证明真实
provider 能登录正确成员。不能通过把租户成员 ID 改成 OIDC subject 来绕过此问题。

官方依据：<https://core.telegram.org/bots/telegram-login>，OpenID Connect 的 Available
Scopes、User Data Structure、Exchange Code for Tokens、Signing algorithm。
公开 discovery：<https://oauth.telegram.org/.well-known/openid-configuration>。

另有两处需要联调时明确：官方手工流程用 HTTP Basic 做 token endpoint 客户端认证，现场
discovery 同时声明 `client_secret_basic` 和 `client_secret_post`，所以当前 form secret
不能仅凭文档示例就判定失败；EdDSA/ES256K 按官方文档只支持 `openid`，而本项目成员绑定
需要 `profile.id`，不能在这种配置下启动登录后才碰到 provider 拒绝。

2026-11-16 的真实回调通过换码和验签后，脱敏诊断确认为 `id` 的 JSON 类型为字符串。
官方示例使用 JSON 整数；现场字符串不能被当前 `as_i64` 读取，导致合法身份在类型检查处
拒绝。本片在既有身份映射修复范围内兼容正整数的规范十进制字符串表示，不改变身份来源、
值域、邀请边界或其他 token 校验。不记录字段值，也不从 `sub` 回退。

## 二、批准后范围

1. 保留 `sub` 为非空、不透明的 OIDC subject；只接受已完成签名、issuer、audience、
   `azp`、expiry、nonce 验证的 token 中的正整数 `id` 作为 Telegram 用户 ID。
   兼容 JSON 整数和规范十进制字符串；字符串只接受 ASCII 数字、无前导零、无符号或空白，
   解析后必须在正 `i64` 范围内。
   不从 `sub`、浏览器参数、未验签 token、手机号或用户名回退推导身份。
2. 更新协议层类型、共享校验函数、所有调用方及桩数据；无需新增表、迁移、成员或 GRANT。
   会话仍调用现有 `issue_web_session_for_user`；未被邀请的用户仍不得签发会话。
3. 对本项目需要 `profile` 的 Web 登录配置拒绝 EdDSA/ES256K；RS256/ES256 继续显式固定算法。
   不按 token header 动态选择算法。配置说明同步，不改变既有会话权限。
4. token exchange 是否改为 HTTP Basic 以官方 discovery 和实测为准；若调整，增加请求形状
   反证，保留超时、禁止重定向、响应大小限制及脱敏错误。
5. 真实联调在独立测试 Bot、受控 HTTPS callback、隔离数据库和显式成员授权下执行。
   不修改现有 Bot 凭据、不登记生产 URL、不调用真实消息发送接口。

## 三、通过判据

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G1/G2、§4.3 WP-6；
`docs/design/eng/03-测试策略.md` 的 OIDC、会话与租户反向测试；
`docs/plan/proposals/PRE-002-P0外部可行性与范围收口.md`。

- 签名 token 中 `sub != id` 时，只绑定 `id` 对应的既有租户成员；不允许 subject 同名成员
  被误登录。JSON 整数和规范十进制字符串必须得到相同成员身份；缺失、非整数、零、负数、
  溢出以及含前导零、符号、空白、Unicode 数字、小数或指数的字符串必须拒绝。
- issuer、audience、`azp`、expiry、nonce、state、browser binding、重放、签名和未知 key
  反证保持拒绝；错误/日志/响应不得出现 code、token、secret 或个人标识。
- 算法与 scope 不相容时离线配置检查拒绝；运行算法始终由服务端配置固定。
- 真实 Authorization Code + S256 + HTTPS 回调完成，会话归属及未邀请拒绝路径有脱敏证据。
  provider 未发生真实换钥时记 `not-run`，不能把当前 JWKS 可读或桩轮换记为真实换钥通过。
- 定向测试、完整 `scripts/ai-driver/ai-gates.sh` 和后续 PR CI 通过。

## 四、输入与边界

需要 BotFather 的 Login Widget Client ID、Client Secret、签名算法、Allowed origin 与完整
callback URI；另需专用测试操作者和已邀请成员的受控标识。Secret 只写服务器受控文件。
租户自办 `api_id` 的脱敏申请记录是独立外部证据，不能由平台现有凭据代替。

本次只读发现及输入检查不等于真实 OIDC 登录通过，也不宣布 G1、G2、G3、G4 或 WP-8 通过。

```text
[report]
task: G1-oidc-provider-alignment-20261116
path: blocked
gates: pass
escalation: -
```
