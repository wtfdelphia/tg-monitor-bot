# PRE-012：CentOS 7 原生验收兼容收口

status: active
wp: WP-8
escalation: yes
allow-paths: scripts/verify-native-candidate.py,scripts/native-release-preflight.sh,scripts/check-native-services.py,deploy/native/README.md,spec/devlog.md
criteria: docs/plan/proposals/PRE-012-CentOS7原生验收兼容收口.md §三

按 2026-11-03 已批准的 PRE-012 修复目标机来源验证器的 Python 3.6 兼容、systemd timer
命令能力判断和配置被忽略时的拒绝检查。保留既有签名身份、来源证明、安装隔离与 unit
加固要求；不升级目标系统，不改变发布 workflow，不放宽 CentOS 7 目标机验收。

实现及本地门禁不等于目标机通过。经 `dev` PR 和 CI 后生成新候选包；新候选复验前不标记
done，PRE-009 与 WP-8 的未完成状态保持不变。
