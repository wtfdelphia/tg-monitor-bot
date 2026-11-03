# PRE-010：GitHub Actions 流水线运行时告警收口

status: active
wp: WP-8
escalation: yes
allow-paths: .github/workflows/ci.yml,.github/workflows/binary-candidate.yml,.github/workflows/binary-release.yml,.github/workflows/docker-publish.yml
criteria: docs/plan/proposals/PRE-010-GitHub-Actions运行时告警收口.md §三

按已批准的 PRE-010 提案，将所有活动 workflow 固定到 `ubuntu-24.04`，把仍运行 Node.js 20
的 Actions 升到经官方元数据确认的 Node.js 24 版本，并将 CI 的 `cargo-deny` 升至 `0.20.2`。
保留权限、触发条件、检查策略、发布身份、目标与工件语义。

按提案验收 CI、cargo-deny、Docker build（不推送 GHCR）与 `dev` 候选签名发布流程；不得触发
正式二进制发布或访问生产服务。远端 CI 裁决通过前不得标记 done。
