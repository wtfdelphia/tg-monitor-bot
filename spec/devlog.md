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
