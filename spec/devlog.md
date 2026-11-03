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
