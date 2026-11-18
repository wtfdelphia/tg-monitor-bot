# 变更验证记录

## 2026-11-02：PRE-008-native-systemd-app-roles

本仓使用 `docs/plan/proposals/` 与 `docs/plan/evidence/` 作为变更和验收事实源，
本记录只说明 `/tmp/AGENTS.md` 要求的等价路径，不复制规格、进度或门禁清单，
不引入第二套 OpenSpec 框架。

Bridge Plan：以已批准的 PRE-008 和合入 `dev` 的 PRE-007 为变更记录，核对原 CLI、
配置白名单和退出链；实现模板、受限环境文件及显式安装/回滚；先跑安装与配置反证，
再跑六角色隔离运行、可用用户 manager 的实际启停和仓库完整门禁；目标 CentOS 7
证据单列为未完成，不用开发机替代。

spec-compliance-check、openspec-verify-change 与 verification-before-completion
按提案 §六逐条对照实现和可运行证据，结论登记在
`../docs/plan/evidence/PRE-008-应用角色原生systemd实施-2026-11-02.md`。
实现只复用既有角色与凭据规则；不改 schema、RLS、API、生产调度或外部账户。

## 2026-11-03：PRE-012-centos7-native-compatibility

Bridge Plan：以已批准的 PRE-012 为变更记录，沿候选安装器、来源验证器、原生预检及两类
unit 的实际调用链核对兼容假设；修正 Python 3.6 语法与 timer 能力检测，保留验签和安全
配置的拒绝边界；先跑既有安装反证和真实 Python 3.6 检查，再跑完整门禁。当前目标机缺少
的加固能力仍为阻塞，不用兼容修复替代运行时验收。

本仓继续用 `docs/plan/proposals/` 与 `docs/plan/evidence/` 等价承载 OpenSpec change，
不引入第二套框架。spec-compliance-check、openspec-verify-change 和
verification-before-completion 对照 PRE-012 §三记录实际命令及未完成项；不改工程卡口
定义、unit 权限、工作流或发布身份。依赖与拒绝说明同步到 `deploy/native/README.md`，
不需要修改项目入口 README、AGENTS 或产品规格。

## 2026-11-17：G1-G2-WP8-G4-closure-followup

Bridge Plan：按维护人本轮批准的收口顺序复用 G1 输入检查器，增加申请日期和既有签署摘要的
只读一致性复核；锁定 G2 待签对象，准备受控确认文件，沿用 WP-8 回填表，列明 G4 取证范围。
申请索引和签署引用按维护人确认更正，原件与更正前版本保留。公开 provider 检查失败单独
报告并保留本地复核结果，不改签署结论、冻结载体、权限、迁移、审计、发布流程或生产配置。

本仓继续以 `docs/plan/proposals/`、`docs/plan/evidence/` 等价承载变更记录。
spec-compliance-check、openspec-verify-change 和 verification-before-completion 对照
本轮授权及既有 G1/G2/WP-8/G4 判据执行：检查一致与不一致输入、私密文件、无网络默认、
不回显凭据、待签哈希和相对链接。结果与剩余外部条件登记在
`../docs/plan/evidence/G1-G2-WP8-G4收口操作清单-2026-11-17.md`。

本轮不改变产品能力、部署路径或关口定义，不需修改入口 README、AGENTS 或冻结规格。
定向测试不替代完整门禁；提交前仍按仓库纪律执行 `ai-gates.sh`，后续 CI 单独裁决。

## 2026-11-17：G2 正式签署及版本冻结

维护人本次明确授权对具体版本作出 G2 正式签署。Bridge Plan：核对当前远端 dev、待签
清单、文件摘要与基线 CI；备份受控待签原件，记录责任方安全语义接受结论及 SDD/TS 冻结；
同步当前清单和历史盘点的后续引用，不改被签署的设计与测试规格内容。

spec-compliance-check、openspec-verify-change 和 verification-before-completion 等价按
`docs/plan/evidence/G2正式安全语义与SDD-TS冻结-2026-11-17.md` 中的具体对象执行：
对照提交和受控文件逐项核对 SHA-256，确认批准来源、时间、备份、权限及开放项一致。
本次仅签署与文档变更，不重复已通过的编译、目标机或 Telegram 验收；此前静态门禁与
基线 CI 分别保留，不声称本轮重新运行完整门禁。入口 README、AGENTS 和冻结载体无需修改。

## 2026-11-18：G1、WP-8、G4 与原生平台边界复核

Bridge Plan：以 `cloud/dev@ae5977dd6cc7b7443da675cc3c0e6a7f5362b927` 和 CI `44205439842`
为当前基线，复核 G1 正式 Go/No-Go、SRS v1.1 候选差异、WP-8 外部/生产输入、G4 剩余出口
以及 Rocky/CentOS 7 原生支持边界。项目内可证明的出口记录为已通过或隔离通过；provider
异常、生产恢复、跨副本留存、告警和值班及自然 FloodWait 仍按证据边界保留 pending/blocked。

本轮新增收口材料：`G1正式Go-No-Go与SRS-v1.1复核-2026-11-18.md`、
`WP-8生产外部与运维证据收口-2026-11-18.md`、`G4剩余出口复核-2026-11-18.md` 和
`原生平台支持矩阵-2026-11-18.md`。新增 `verify-external-closure-evidence.py` 及其
回归测试，统一校验 G1/WP-8/G4 的脱敏外部证据输入；缺少 provider、生产或自然事件证据时
只返回 `blocked`。不修改迁移、RLS、GRANT、CI、生产配置、timer 或 Telegram 测试行为；不
重复 Rocky 验收，不主动制造 FloodWait，不执行生产恢复或真实租户清理。

本轮同时复核 Telegram 官方 OIDC 与测试环境文档：公开文档没有定义 OIDC 专用的换钥、未知
`kid`、JWKS 故障或过期 token 注入接口，普通测试 Bot 和公开 JWKS 读取不能替代这些外部证据。

随后收紧外部关闭证据校验：G1 provider 项和 G4 FloodWait 项无论处于 `pass`、`not-run` 或
`blocked`，都必须明确记录 `active_probe=false`；缺失或主动探针标记会被拒绝，避免把主动
制造的事件混入未决证据。

本轮 G1 追加测试直接覆盖现有生产验签函数和 callback：模拟 old-only、old+new、new-only 的
JWKS 衔接，以及未知 `kid`、JWKS 503、过期 token 三类失败回调不签发会话、消费 state、拒绝
重放和错误脱敏。只修改既有测试区域并复用 RSA/JWKS helper，不改认证实现或冻结 TS 文档。
专项 OIDC 回归通过；公开文档、discovery、JWKS 的只读响应以哈希归档在受控目录。单快照
不证明换钥，合成 token 和本地 HTTP 不替代真实 provider 事件，四类外部项仍为 `not-run`。

G1 复核记录补齐 SRS 两个载体的基线和完整 SHA-256，并准备受控待签包。Bridge Plan 与
spec-compliance-check 对照既有 G1 批准提案逐条核对身份来源、错误拒绝及证据边界；
openspec-verify-change 与 verification-before-completion 采用专项测试、clippy、格式和
文档摘要核对。责任方未接受本轮具体证据前不代填签名、Go 或冻结日期；原签署保持原样。
入口 README、AGENTS、SRS/SDD/TS 正文无需修改，本次不引入新产品能力或卡口。

本轮最终验证：OIDC 专项 `17 passed`，G1 输入/预发布脚本回归 `23 passed`，外部关闭清单回归
`9 passed`；定向 clippy、fmt、`git diff --check` 通过；本轮修改后的
`ai-gates.sh --static-only` 通过，数据库卡口未运行。日志、测试 diff 和 SRS 两个载体按 SHA-256
归档在受控目录，测试专属 target 在确认进程结束后清理。未提交、推送或创建 PR。

## 2026-11-18：G1 正式裁定与 SRS v1.1 冻结

维护人明确接受待签包内四类客户端处理行为的隔离证据，并授权按包内具体版本作出责任方
裁定及符合条件的 SRS 冻结。Bridge Plan：重新核对待签包、测试源码、原始日志、归档 diff、
租户材料和 SRS 两个载体的哈希；保留历史 No-Go/待签材料并生成本次正式签署；按已允许的
`approved-isolation` 来源填写关闭清单，统一当前文档引用。

按本次明确授权记录责任方 `WTFGEDelphia` 于 `2026-11-18T12:18:22+08:00` 的签署登记：
G1 正式 Go，SRS v1.1 两个具体载体冻结，四类真实 provider 异常继续 `not-run`。同 DC QR 和
X1–X5 沿待签包保留延期状态；技术可行性收口不等于 G3/G4/WP-8 或生产部署放行。

spec-compliance-check、openspec-verify-change 和 verification-before-completion 等价按
G1 当前签署记录核对批准原文、具体对象、哈希与证据类别，运行关闭清单校验和既有脚本回归。
本次仅记录已授权的裁定和冻结，不修改 SRS/SDD/TS 正文、生产代码、CI 或数据库；沿用前次
实际通过的 OIDC 专项及静态门禁，数据库卡口未运行，不重复构建或目标机/Telegram 验收。
入口 README 与 AGENTS 无需更改；本批继续保留在工作树，等待用户提交/PR 指令。

签署后实际检查：G1 关闭清单返回 `pass/go/signed/frozen`；申请材料一致性检查通过且不代判
签署；外部关闭清单回归 `9/9`、G1 输入/预发布回归 `23/23` 通过。SRS、测试归档、旧签署与
待签包摘要匹配，受控文件均为 `0600`；相对链接、围栏及 `git diff --check` 通过。验证摘要
保存在受控的 `G1-signoff-verification-2026-11-18.json`，本轮未生成 Rust 构建缓存。

## 2026-11-18：G3、WP-8、G4 统一收口复核

以 `cloud/dev@6809880ebad1f213ec8fd50839381691b28f199f` 和 CI `44254674389` 为当前基线，
将用户要求的 WP-8 产品/法务、生产恢复、RPO/RTO、告警和值班、跨副本留存，G4 生产指纹
重算与自然 FloodWait，以及 G3 总体出口拆成可核对输入。扩展 `verify-external-closure-evidence.py`
支持 `kind=g3`：通过项必须有脱敏引用，未通过项必须写原因，避免把 WP-8 缺口静默折算为 G3 通过。

新增统一收口记录 `G3-WP8-G4统一收口复核-2026-11-18.md`。当前 G3、WP-8、G4 仍为
`blocked`；已存在的非生产端到端、RLS 两态和历史门禁证据继续复用。未连接生产服务、未启用
timer、未删除真实租户、未重复 Rocky 验收、未主动制造 FloodWait。没有自然 FloodWait 时，
真实闭环继续保持 `partial/not-run`；生产环境测试和稳定发布继续延期。

复核补充：WP-1 的迁移、角色权限、受限角色隔离测试与 RLS 双遍审计均有现有实现或 CI 证据；
汇总按 §4.3 记为出口判据证据通过，同时注明未找到独立 WP-1 出口记录。该归档缺口不额外
升级为未满足判据。
G4 对客审阅稿标题改为“服务边界说明”，避免在规格要求对外使用“上线”一词的情况下，
把“首发”放入对客标题；法务审阅和分发授权仍未完成。

### G3/WP-8 外部关闭证据校验器修复

用户批准 `G3-WP8-closure-validator-hardening` 后，校验器要求 RPO/RTO 为带 `ms/s/m/h/d`
单位的非负时长，拒绝说明文字；跨副本系统数组逐项校验，畸形元素返回 `ManifestError`；
G3 校验要求同时提供 WP-8 清单，并拒绝 G3 中 WP-8 状态与 WP-8 证据门矛盾。同步更新外部
输入规范，说明格式限制及命令参数约束。

`PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts.tests.test_external_closure_evidence -v`
通过，26 项测试全绿；Python 语法检查与 `git diff --check` 通过。测试只验证结构和状态处理，
不验证外部引用真实性，不连接生产，也不改变 G3、WP-8 或 G4 的阻塞状态。
