# G1 正式 Go/No-Go 与 SRS v1.1 复核（2026-11-18）

任务：`G1-formal-go-no-go-and-srs-v1.1-review-20261118`。

复核基线：`cloud/dev@ae5977dd6cc7b7443da675cc3c0e6a7f5362b927`。
合并后 CI：`44205439842`，结果为 `success`。既有基线与本轮新增的未提交回归分别记录；
不把维护人确认、隔离测试或公开 JWKS 可读扩大为真实 provider 异常证据。

当前状态：`G1=Go; formal-signoff=signed-by-maintainer; SRS-v1.1=frozen`。
依据责任方 `WTFGEDelphia` 本轮对待签包具体版本的授权，于
`2026-11-18T12:18:22+08:00`（Asia/Shanghai）登记正式裁定与冻结，并记录对四类客户端
处理行为隔离证据的接受。签署前逐项复核
待签包、SRS 载体、测试源码、日志及归档哈希，全部匹配；四类 Telegram 真实异常仍为 `not-run`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G1；
`docs/plan/proposals/G1-OIDC真实身份映射与联调.md` §二、§三；
`docs/plan/evidence/G1正式Go-No-Go复核-2026-11-17.md`。

## 一、六项 G1 判定

| 验证项 | 当前结果 | 证据边界 |
|---|---|---|
| grammers 连通与消息接收 | `pass（既有真机记录）` | 复用既有消息接收、catch-up 和恢复记录，不重复 Telegram 真机验收 |
| QR 登录全分支 | `pass（既有记录；同 DC 子分支未单独证明）` | 2FA、`MigrateTo`、过期 token、session 恢复和撤销已有记录；同 DC 正常扫码没有独立记录 |
| 账号租约原型 | `pass（单实例/项目内测试）` | fencing 与并发抢占已有测试；多实例 X1–X5 按单实例部署决策保持 `not-run` |
| 媒体中转可行性 | `pass（既有真机与隔离记录）` | 已有上传、下载、中转和清理记录，不等于生产容量通过 |
| Login/OIDC 前置 | `pass（真实正常登录与批准的隔离故障覆盖）` | 复用真实 OIDC 登录与 `NOT_INVITED` 记录；四类客户端处理行为按本次责任方接受的隔离回归收口；四类真实 provider 异常仍未发生或未形成证据，不声称已验 |
| `api_id` 归属模式 | `pass（责任方正式接受）` | 申请日期、当前摘要及旧材料替代关系已核对，并按本次签署纳入 G1；不以脚本一致性代替申请事实 |

## 二、正式 Go/No-Go

正式裁定：**Go**，G1 技术可行性通过。

责任方本次接受待签包中的具体证据和版本，完成以下处置：

1. 真实 Authorization Code + S256 + HTTPS 回调、成员映射、会话签发及未邀请拒绝复用既有
   证据；四类客户端处理行为由第七节的隔离回归覆盖，证据来源记录为 `approved-isolation`。
   该接受只关闭客户端处理行为的验收，不证明 Telegram 曾发生换钥或故障。
2. 目标租户材料、实际申请日期 `2026-09-25`、当前摘要及旧材料替代关系纳入正式签署。
   **`api_id` 归属模式已确认为租户自办。**
3. 同 DC QR 子分支沿待签包和既有责任方处置继续延期，保持 `not-verified`；X1–X5 按单实例
   部署决策保持 `not-run`，不因 G1 Go 改写逐项实测状态。
4. 以第八节两个载体的完整哈希冻结 SRS v1.1；第三节的实现说明是评审附件，不修改载体。

这是对列明范围的正式技术可行性裁定，不是“带条件通过”。真实 provider 事件的观察边界、
延期分支及后续 CI 独立保留；未提交测试尚未经过 PR CI，基线 CI 不能替代该次检查。G3、G4、
WP-8、种子客户与生产接入仍须满足各自出口和授权，本次签署不放行这些事项。

## 三、SRS v1.1 候选内容

以下候选说明已经纳入本次评审，记录已实现或已由基线规格确定的语义，不新增产品承诺。
SRS v1.1 冻结载体为第八节的两个文件，以下说明不作为新增规格正文或第三个冻结载体：

1. Telegram OIDC 成员绑定使用已验签 token 中的 `profile.id`；`sub` 保持不透明，不能作为
   成员 ID 回退来源。`profile.id` 接受正整数及规范十进制字符串，其他格式拒绝。
2. 需要 `profile` 的 Web 登录只允许配置 `RS256` 或 `ES256`；不根据 token header 动态
   选择算法，`EdDSA` 与 `ES256K` 不作为该登录配置的兼容声明。
3. `api_id` 归属模式确认为租户自办；平台不使用共享的平台应用凭据替租户采集。按照
   ADR-0013，平台仍保存租户凭据的加密可解副本；不能将“不代持”理解为“不存储”。申请材料
   和凭据安全标记只通过受控证据引用，不进入规格正文或仓库。
4. G1 的四类客户端处理行为与真实 provider 事件分开记录；前者为本次批准隔离覆盖的
   `pass`，后者继续 `not-run`。目标租户材料由责任方正式接受，同 DC 子分支仍延期。
5. 本次候选差异不改变首发/上线/商用定义、Telegram 外呼边界、租户隔离、凭据处理或
   G4/WP-8 出口判据。

## 四、SRS v1.1 正式冻结条件

SRS v1.1 的本次冻结条件已逐项满足：

- 四类客户端异常路径已有责任方接受的隔离证据，具体对象见第七节；
- 目标租户自办 `api_id` 材料的申请结果、复核记录及当前哈希已纳入责任方签署；
- G1 责任方接受候选说明、证据边界、开放项并授权正式 Go/No-Go 裁定；
- 同 DC QR 状态与延期处置已按待签包保留，未改记为已验；
- 第八节两个 SRS 载体与待签包、基线和归档字节一致。

当前状态：

```text
G1_formal_decision=Go
G1_formal_signoff=signed-by-maintainer
SRS_v1.1=frozen
client_failure_evidence=pass-approved-isolation
telegram_provider_events=not-run
tenant_api_id_final_signoff=accepted
same_dc_QR=deferred-not-verified
X1-X5=deferred-not-run
```

## 五、2026-11-18 受控材料复核

签署前在不执行 env 文件、不重复真实登录的前提下运行：

```text
input_readiness=ready
tenant_evidence_file=available-consistency-checked
tenant_evidence_review.status=consistent-pending-review
provider.discovery=pass
provider.jwks_fetch=pass
provider.provider_rotation=not-run
formal_signoff=not-assessed
```

配套的 `scripts/verify-external-closure-evidence.py` 只接受 `0600` 脱敏 JSON；它要求四类
provider 外部证据、`api_id` 复核、正式签署和 SRS v1.1 哈希全部到位，缺任一项只返回
`blocked`。本次签署后已生成受控清单 `/home/openclaw/.config/tgm-closure/g1.json`，四类项的
`source` 均为 `approved-isolation`，另将 `telegram_provider_events` 逐项保留 `not-run`。
校验器仍只检查结构与声明；正式接受来源是本轮维护人授权，不是脚本自动产生签署。

## 六、官方 provider 能力边界复核

2026-11-18 查阅 Telegram 官方登录文档和 Bot 测试环境说明。登录文档定义了 OIDC discovery、
Authorization Code + PKCE、token endpoint、JWKS 和签名算法；Bot 测试环境说明提供与生产隔离的
测试账号/测试 Bot 方式，但没有定义 OIDC 专用测试端点，也没有提供可由租户控制的换钥、未知
`kid`、JWKS 故障或过期 token 注入接口。

因此，普通测试 Bot、公开 JWKS 读取和一次正常登录不能形成四类 provider 异常证据。可接受的
补证来源包括 provider 正式提供的隔离记录，或责任方批准且可核对的隔离证据。本次接受
第七节材料覆盖客户端行为；真实 provider 事件仍为 `not-run`，不重复登录或主动破坏 provider。

官方依据：

- <https://core.telegram.org/bots/telegram-login>
- <https://core.telegram.org/bots/features>

## 七、本轮隔离故障回归与公开取证

在 `crates/app/src/control.rs` 的既有测试模块补齐两组回归，直接调用当前生产验签函数与
callback router。HTTP 服务只监听 `127.0.0.1` 的临时端口，使用一次性 RSA 测试密钥、合成
token 和惰性数据库池；没有向 Telegram 发送登录、换码、消息或故障请求。

| 场景 | 实际验证 | 本轮项目内证据 | Telegram 真实异常 |
|---|---|---|---|
| rotation | old-only → old+new → new-only 三份 JWKS；新旧 token 接受矩阵依次为 `[true,false]`、`[true,true]`、`[false,true]`；退役 key 拒绝 | `pass` | `not-run` |
| 未知 `kid` | 有效签名 token 与另一把有效公开 key 不匹配；callback 返回 503，不签发会话，state 已消费，重放返回 403 | `pass` | `not-run` |
| JWKS 故障 | token endpoint 成功、JWKS 返回 503；callback 返回 503，不签发会话，state 已消费，重放返回 403 | `pass` | `not-run` |
| 过期 token | 签名、key 与其他 claims 正常，`exp` 已过期；callback 返回 400，不签发会话，state 已消费，重放返回 403 | `pass` | `not-run` |

后三类错误响应同时检查不包含测试 token、state、code 或 Client Secret。可运行入口：

```bash
SQLX_OFFLINE=true cargo test -p tgm-app --lib control::tests::oidc_ -- --test-threads=1
```

本轮结果为 `17 passed; 0 failed`，原始日志保存在
`/tmp/tgm-g1-oidc-regression-20261118.log`。测试使用真实密码学签名，但材料是合成的；这些
测试不构成 Telegram 签发过期 token、返回未知 key、发生故障或轮换密钥的记录。责任方本次
已接受这些具体材料作为四类客户端处理行为的隔离证据；四类真实事件状态不改写。

本轮还通过 `cargo clippy -p tgm-app --lib --tests -- -D warnings`、格式检查、G1 输入脚本
回归和外部关闭清单回归；`ai-gates.sh --static-only` 的静态卡口全部通过，数据库卡口未运行。
以下摘要绑定本轮实际运行对象和日志，不将基线 CI 解释为本轮未提交改动已经通过 CI。

| 对象 | SHA-256 |
|---|---|
| `crates/app/src/control.rs` | `696902767960ff26576d69d0a802f86aef73fda0c29b32b1d1523b99f0e5c90f` |
| `/tmp/tgm-g1-oidc-regression-20261118.log` | `1517331b3af96a623c3fad67a4c2fd6971f91b1a18f5714f401974eff3e3e189` |
| `/tmp/tgm-g1-oidc-clippy-20261118.log` | `13ef08fa364678015c85af01c66ab3cba4e574454264eb3d861029802d63770b` |
| `/tmp/tgm-g1-oidc-static-gates-20261118.log` | `13cf9a7f1c5fe69f26dd5e5bb3d898ce320f0d73a8b095174077efdc0f81c549` |

日志、测试 diff 和两个 SRS 载体另存于受控目录
`/home/openclaw/.config/tgm-g1/provider-evidence-20261118/isolated-client-regression/`，归档
`verification.json` 的 SHA-256 为
`fff4a28f599e6afbe106d760bfbcfebbd7a805f6fe2b63b50a784ddb86308791`。
该归档保留签署前原始结果与 `external_evidence_acceptance=pending`，不随临时构建缓存清理
而删除，也不回写历史清单。后续正式接受记录以第八节签署文件及受控关闭清单为准。

本轮另于 `2026-11-18T03:22:05Z` 至 `03:22:07Z` 只读保存官方登录文档、discovery 和 JWKS。
三次 HTTPS 请求均为 200，没有发送凭据。原响应与观察清单保存在
`/home/openclaw/.config/tgm-g1/provider-evidence-20261118/`，目录权限 `0700`、文件权限 `0600`。

| 公开原响应 | SHA-256 |
|---|---|
| `telegram-login.public-response` | `c2e4096b7201a6a882588e7488bfd291918ed84a37bdcf51962a31e5b5b3e947` |
| `discovery.public-response` | `36dd6c958d93cfe5a4e8e1796eddf2541c85b750209fe0a3e4544a7563fed792` |
| `jwks.public-response` | `ff43610c7cca372000fd6a9eed14695e8af9d70f65aabc00fdf5694588fe2e48` |
| `observation.json` | `1debaeaf21a6036898de5bab78909159911d974da05608f73d104b7021051a6e` |

JWKS 本次包含 4 个公开 key；`observation.json` 保存按规范化 key 对象计算的摘要。该快照可作
日后自然换钥的对照，单次观察不能证明发生换钥，也没有设定轮询或后台观察任务。

## 八、SRS v1.1 待签对象与责任方操作

SRS 的载体按 `plan/00` §三 G0/G1 保持为下列两个文件；本轮不修改其内容，签署前确认实际
字节与待签包、基线及归档完整摘要一致。第三节为与实现对应的评审附件，不增加 SRS 产品
范围；后续修订按 `plan/00` §九分级并重新签核。

| 文件 | 基线提交 | SHA-256 | 状态 |
|---|---|---|---|
| `docs/design/spec/00-范围与非目标.md` | `ae5977dd6cc7b7443da675cc3c0e6a7f5362b927` | `95adfa951e9274a3dd1614f7e4d7baf5db5132cbb387c3fe40fcc6006ccbd747` | `frozen` |
| `docs/design/spec/01-产品需求.md` | `ae5977dd6cc7b7443da675cc3c0e6a7f5362b927` | `a6554e975f6a8341c27327a7be04b6b8af437a31b5d6bdac37cd0867e684ffe9` | `frozen` |

受控待签包 `/home/openclaw/.config/tgm-g1/G1-signoff-ready-2026-11-18.md` 保持原样，SHA-256 为
`ee8fd731e280e376153d0a692b89017cd4fe91f4898184ab467b5b60b6ccdc1b`。原 `G1-signoff.md`、
待签包和签署前复核正文的副本保存在
`/home/openclaw/.config/tgm-g1/G1-signoff-backup-20261118-_l09afna/`，旧 No-Go 记录不覆盖。

正式签署文件：`/home/openclaw/.config/tgm-g1/G1-signoff-2026-11-18.md`，权限 `0600`。
它承接原 `G1-signoff.md`，作为本次裁定的当前记录；旧文件保留历史用途。维护人在本任务中
明确授权：

> 接受待签包中的隔离证据覆盖这四类客户端处理行为，并授权按包内具体版本完成正式责任方裁定及符合条件的 SRS 冻结

```text
签署记录编号：G1-Go-SRS-v1.1-20261118
责任方：WTFGEDelphia
批准来源：维护人在本任务对话中的上述明确授权
签署登记时间：2026-11-18T12:18:22+08:00（Asia/Shanghai）
G1 正式裁定：Go
四类客户端行为证据：accepted-approved-isolation
四类 Telegram 真实异常：not-run
SRS v1.1 冻结：frozen
冻结日期：2026-11-18，Asia/Shanghai
```

签署只覆盖本节对象与第二节的技术可行性，不授权生产操作、种子客户接入、`main`/GHCR
发布或稳定版本发布。G4、WP-8 及其他工作包仍独立裁定，不因本次签署改变其出口。

签署后核对结果：受控关闭清单返回 `overall=pass`，其中 G1 为 `go/signed/frozen`；G1 输入
检查确认申请日期和当前证据哈希一致，仍如实返回 `formal_signoff=not-assessed`，因为该脚本
不判断签署。外部关闭清单回归 `9/9`、G1 输入/预发布回归 `23/23` 通过；本轮未改代码，
沿用第七节测试与静态门禁结果，不重复构建或重新登录。

核对清单：`/home/openclaw/.config/tgm-g1/G1-signoff-verification-2026-11-18.json`，权限 `0600`；
正式签署文件 SHA-256 为
`858eac88870a0e89e2db5fe2f7925b64d889ad5625d6ebc94bc933d54032e8e8`，受控关闭清单 SHA-256 为
`070d44d72800ef31cbf4e0ed106a143edda3f4fa9663836e691ac5203860d51a`。

```text
[report]
task: G1-formal-go-no-go-and-srs-v1.1-review-20261118
path: ok
gates: pass
escalation: -
```
