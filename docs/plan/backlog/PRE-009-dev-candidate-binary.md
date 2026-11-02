# PRE-009：dev 候选二进制验收包

status: active
wp: WP-8
escalation: yes
allow-paths: .github/workflows/binary-candidate.yml,scripts/native-release-package.py,scripts/install-native.sh,scripts/verify-native-candidate.py,scripts/check-native-services.py,deploy/native/README.md
criteria: docs/plan/proposals/PRE-009-dev候选二进制验收包.md §三

按已批准提案新增独立候选发布 workflow。只从 `dev` 的精确提交构建，拒绝 CI 未通过的
提交；输出标记为非稳定版的候选包，并使用独立 GitHub Pre-release、Sigstore keyless
签名及来源证明。构建、签名和 Release 写入使用独立 job 权限。不得修改稳定发布流程、
Docker 镜像发布流程或 `ci.yml`。

候选安装必须由操作者显式确认，只允许非 root 用户隔离安装，不切换稳定版命令链接，不安装
或启用 systemd 服务。稳定安装的默认身份与行为保持不变。

候选 workflow 文件须先进入 GitHub 默认分支后才能由 `workflow_dispatch` 选择 `dev` ref；
进入默认分支只启用候选验收能力，不表示创建稳定版。

候选包打包器、来源验证器、安装器拒绝路径和安装隔离均有可运行反证。PR 合并后还需手动
触发 workflow 完成签名发布演练，并在非生产 CentOS 7 目标机留存验收证据；这两项完成前，
本任务不能标记 done。完整卡口以 CI 对该任务分支的裁决为准。
