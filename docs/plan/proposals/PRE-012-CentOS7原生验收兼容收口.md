# PRE-012：CentOS 7 原生验收兼容收口

任务 id：`PRE-012-centos7-native-compatibility`。

状态：2026-11-03 用户批准实施；本地验证及目标机复验结果单独登记，不代表验收通过。

## 一、背景与目标

PRE-009 候选包已完成签名发布演练。目标机回传清单和 provenance 验签通过、候选资产摘要
校验通过，但实际环境是 Python 3.6.8、systemd 219，systemd 构建特征含 `-SECCOMP`。
现有来源验证脚本使用 Python 3.7 才支持的 future 特性，原生预检还无条件调用目标机没有的
`systemd-analyze calendar`。这两处是项目对已声明目标平台的工具兼容遗漏，不应要求操作者
临时改包、替换系统 Python 或升级系统 systemd 来掩盖。

还需核对 unit 的实际安全能力。cleanup unit 使用 `ProtectSystem=strict`，cleanup 与 root
应用 unit 使用 `RestrictAddressFamilies`；上游 systemd 219 的对应能力与当前目标机存在差异。
二进制 ABI、工具可运行、unit 语法和运行时加固是不同判据，不能合并成一个“兼容”。

判据出处：`docs/plan/proposals/PRE-007-二进制生产发布.md` 的目标平台与安装检查；
`docs/plan/proposals/PRE-008-应用角色原生systemd服务.md` §二、§三；
`docs/plan/proposals/PRE-009-dev候选二进制验收包.md` §三；
`docs/design/eng/04-CI门禁.md` 的失败即拒绝与平台校准纪律。
复现见 `docs/plan/evidence/PRE-009-CentOS7目标机兼容阻塞-2026-11-03.md`。

## 二、批准后实施范围

### 1. 保持验证语义的 Python 3.6 兼容

复用现有 `scripts/verify-native-candidate.py`，审查语法和所用标准库 API，以最小改动支持
目标机 Python 3.6.8，不引入依赖、不下载解释器、不修改目标机系统组件。原有 issuer、精确
workflow identity、清单集合、包内外 BUILDINFO/SBOM、provenance、源码 SHA、运行标识和
归档摘要校验全部保留。

兼容范围仅是目标机安装验证工具，不把 Python 3.6 定为整个项目或 CI 的运行基线。

### 2. 按能力验证 timer，错误仍须拒绝

修正 `scripts/native-release-preflight.sh` 及其现有测试。支持 `calendar` 的环境保留独立
日历解析；不支持的环境必须让目标机本身的 `systemd-analyze verify` 解析 timer 的
`OnCalendar`，同时检查退出状态和本任务 unit 的解析诊断。空值、无有效定时项、非法日历、
未知字段及被忽略的字段值都须拒绝，不能用 `|| true` 或仅跳过子命令处理。

cleanup 与应用路径复用已有预检入口，对安全字段的拒绝规则保持一致。当前公开安装参数、
稳定版链接及候选安装隔离不变；不新增跳过预检或弱化验证的开关。

### 3. 明确报告不具备的原生服务安全能力

采集目标机 systemd 完整包版本、编译特征和 unit 解析结果，兼顾发行版回移，不能仅按主版本
判定全部能力。对 `ProtectSystem`、地址族限制、应用重启限速等现有配置验证实际支持情况；
发现未知、忽略或编译期缺失时明确拒绝相应原生服务验收，不依赖 parser 的成功返回码。

本提案不授权把 `ProtectSystem=strict` 改成 `full`，不删除 `RestrictAddressFamilies`，
不关闭沙箱或增加 privileges。若目标机确实不能满足现有加固要求，保留 CentOS 7 / glibc
2.17 的二进制兼容目标，但原生服务仍不得标绿；等价补偿措施、另选运行平台或降低安全基线
必须另提方案并获批准。只完成工具兼容不能承诺当前主机可以完成原生安装。

### 4. 增补检查并生成新的候选包

扩展 `scripts/check-native-services.py` 的既有反证，覆盖 Python 3.6.8、无 `calendar` 的
systemd、解析器返回成功但忽略安全字段或非法日历、缺少 SECCOMP、正常现代环境和候选隔离。
真实 Python 3.6.8 运行检查与模拟命令测试分开记录；模拟通过不等于目标机能力已通过。

文档同步目标机依赖与拒绝原因，不更改工程卡口定义及 workflow 权限、触发器或签名身份。
实现经以 `dev` 为 base 的 PR 和 CI 后，从新提交重跑既有候选 workflow，生成新 tag、新
Pre-release 和签名；不覆盖或重新打包旧候选资产。

## 三、验收判据

1. 来源验证器在真实 Python 3.6.8 解释器中可运行；原有篡改、错误身份、摘要、提交及运行标识
   的反证继续拒绝，不能只以 `--help` 成功替代完整验证。
2. systemd 没有 `calendar` 时，有效日历仍由本机解析；非法、空值、被忽略的配置即使 parser
   返回零也失败；现代环境的独立日历检查不回退。
3. 本任务 unit 的未知安全字段、值被忽略或必要编译能力缺失时预检拒绝；单独满足 glibc 2.17
   不会被报告成服务安全验收通过。
4. 候选安装仍只允许非 root 显式接受候选，不安装 unit、不切换稳定链接、不执行迁移、
   不启动或启用服务，不连接生产数据库、Telegram、KMS、Redis 或 S3。
5. 现有原生工件检查及完整本地门禁通过，最终结果由 PR CI 裁决；新候选目标机结果单独留证。
   系统组件或运行时能力不满足时，明确记录阻塞，不宣布 PRE-009 或 WP-8 完成。
6. 检查结束只清理本任务的临时镜像、容器及目录，不删除用户候选包、证据或共享环境。

## 四、不在范围

- 不升级或替换目标机 Python、systemd、内核、OpenSSL 或发行版。
- 不自动降低 unit 安全配置，不改变 root/user 的权限和凭据边界。
- 不修改 `ci.yml`、发布 workflow 权限、签名身份、GHCR 推送或 `main`。
- 不创建稳定 Release，不覆盖旧候选 tag，不编辑操作者已下载包中的签名保护内容。
- 不安装生产包、不启用 timer、不执行真实租户清理，不把验签成功视为 WP-8 出口通过。

## 五、批准后交付顺序

1. 先修复 Python 工具兼容，保留并复跑来源校验与反证。
2. 修正 systemd 能力判断和 timer 验证，补齐安全字段被忽略的拒绝检查。
3. 运行定向检查与完整门禁，审查 diff 后按一个提交开往 `dev` 的 PR；提交与发布沿用用户授权边界。
4. 合入并通过 CI 后生成新的签名候选包，由操作者重跑目标机非生产验收。
5. 如实际安全能力仍不满足，留在阻塞状态，单独提出补偿或平台方案，不自行更改通过标准。
