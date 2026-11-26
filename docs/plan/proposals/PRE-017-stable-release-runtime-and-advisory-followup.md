# PRE-017：稳定发布工作流运行时与依赖公告后续

任务 id：`PRE-017-stable-release-runtime-and-advisory-followup`。

状态：维护人于 2026-11-26 批准（「批准提案 开始实施」）。§3.1 已随
PR #141 合入 `main`（commit `8c10c2c`）。§3.2 的维护人风险裁定已于
2026-11-26 记录（`docs/plan/evidence/PRE-017-v010-url公告风险裁定-2026-11-26.md`）；
新稳定版本动作仍按 §四 审批分界等待独立批准。

## 一、背景

`main` 上 `binary-release.yml` 的 `prepare` 与 `publish` job 使用 `actions/upload-artifact@v4` 和 `actions/download-artifact@v4`。正式版发布 run `44969396475` 的 `publish` job 对 `download-artifact@v4` 明确报告 Node.js 20 弃用警告；`upload-artifact@v4` 的官方 metadata 也声明 `node20` runtime。当前候选 workflow 已使用 `upload-artifact@v6` 与 `download-artifact@v7`。PRE-010 的运行时收口证据形成早于 `binary-release.yml` 进入默认分支后的首次稳定发布运行。

正式发布 `v0.1.0` 的 SBOM 和源码锁文件包含 `url 2.5.8`，对应 `RUSTSEC-2026-0592`。公告注明受影响范围为 `url <= 2.5.8`，修复版本为 `2.5.9`。当前 `main` 与 `dev` 的 `Cargo.lock` 已更新至 `2.5.9`；该更新没有改变已发布的 `v0.1.0` 资产。应用源码未发现公告所列 API 调用。2026-11-25 对正式二进制正常依赖图及缓存源码进行的定向检索见 §6.2.1；源码检索没有发现 `url::Url::origin()` 或 `url::quirks::origin()` 的调用，但没有构建跨 crate 调用图，不能据此裁定漏洞路径在二进制中不可达。

证据：`docs/plan/evidence/PRE-010-PRE-013-GHCR首次发布-2026-11-23.md`；GitHub Actions run `44969396475`；PR #132；GitHub advisory `RUSTSEC-2026-0592`；GitHub Release `v0.1.0` 的 SBOM 与 `BUILDINFO.json`。

判据出处：`docs/plan/proposals/PRE-010-GitHub-Actions运行时告警收口.md` §三；`docs/plan/proposals/PRE-013-默认分支稳定发布注册.md` §3.3、§3.4、§四；`docs/design/eng/04-CI门禁.md`。

## 二、目标

1. 将稳定发布 workflow 的 artifact Actions 更新到当前受支持的 Node.js 24 runtime，消除已观察到的 Node 20 弃用告警。
2. 为未来稳定版本建立依赖公告检查和审批要求，使新的稳定资产使用已经通过 `cargo deny check advisories` 的锁文件，并完成对既有 `v0.1.0` 的可达性评估与维护人风险裁定。
3. 明确 `v0.1.0` 是不可变的历史发布，后续修复必须生成新版本和新签名，不改写旧 tag、Release 资产或审批记录。
4. 保留 `main` 精简稳定树清单、显式审批、签名、provenance、SBOM 和稳定发布权限边界。

## 三、批准后实施范围

### 3.1 稳定发布 Actions runtime

- 将 `binary-release.yml` 中 `upload-artifact@v4` 升至 `@v6`，将 `download-artifact@v4` 升至 `@v7`，并核实官方兼容要求。
- 保持 `prepare` / `publish` 两阶段、WTFGEDelphia 精确发布审批、精确 tag/source 校验、权限隔离、不可覆盖检查、工件路径和保留期语义。
- 核对当前 CI 与候选 workflow 的版本选择，避免重复引入较旧的 artifact Action。
- 新增下载验证时将其作为独立的非发布运行路径，确认它只读取指定 prepare run 的精确工件，且不进入 `publish` job 或取得发布权限。
- 更新精简稳定树清单和对应反证测试，证明更新后的 workflow 仍被显式允许并满足 `--strict`。

### 3.2 后续稳定版本的依赖公告处理

- 新稳定版本候选来源只能是满足 G4 条件并完成稳定评审的 `dev` 提交经批准晋级到 `main` 后的精确提交。
- 在新的 `prepare` run 中核对 `Cargo.lock` 锁定 `url >= 2.5.9`、`cargo deny check advisories` 通过、SBOM 反映新锁文件，并附上既有 `v0.1.0` 风险裁定引用。
- 重新生成二进制、源码归档、校验清单、签名、provenance、SBOM 和审批 statement，并逐一登记 SHA-256。
- 由维护人记录 `v0.1.0` 的风险处置结论。Git tag、Release 资产、签名和来源证明保持原样，不原地替换或重签旧资产。

### 3.3 验收

- PR CI 的 `static`、`database` 和 Docker build validation 全绿。
- 稳定树 checker 的宽松模式和 `--strict` 检查通过，artifact workflow 路径仍为显式登记项。
- `prepare` 只负责验证 Node.js 24 `upload-artifact` 上传与准备工件；另用独立、无发布权限的验证运行，按精确 prepare run ID 和工件名调用 `download-artifact@v7` 并比较下载结果。该验证不得进入 `publish` job，也不得持有 `contents: write` 或 OIDC 发布权限。
- 核对工件名称、文件清单、90 天保留期、`if-no-files-found: error`、下载目录结构及准备记录摘要在迁移前后保持一致。
- 无 Node.js 20 deprecation、`ubuntu-latest` migration 或依赖公告失败告警。
- 不因本提案运行生产迁移、安装生产服务、发送 Telegram 消息或启用清理 timer。

## 四、审批分界

本提案批准后可以实施 workflow 与门禁更新，也可以运行不发布 Release 的 `prepare` 验收。创建新稳定 tag、运行 `publish`、生成新正式 Release 或推送稳定 Docker 镜像仍需针对精确 source commit、版本和工件摘要的独立明确批准。

新稳定版本继续受 PRE-013 顺序条件约束。G4 未通过时，不得以 PRE-017 代替 G4，也不得把 `v0.1.0` 的既有签名结果当作修复后锁文件的发布证明。

## 五、停止条件

- artifact Action 官方版本或 runner runtime 的兼容范围无法核实。
- 稳定树核对器无法在不降低严格白名单语义的情况下登记 workflow。
- `cargo deny check advisories`、SBOM、provenance、源码归档或稳定签名链无法绑定到同一精确提交。
- 依赖公告评估无法确认新锁文件包含修复，或维护人尚未裁定已发布 `v0.1.0` 的风险处置。
- 为生成新稳定资产而需要提前绕过 G4、PRE-013 或显式审批流程。

## 六、2026-11-24 至 2026-11-25 技术复核增量

核对基线为 `cloud/dev@994ab82a2f941ec50bc37146fbfb16d5ff6758c1` 和 `cloud/main@efab6cdac6d89d401ac712bebb826a9dff187169`。dev CI `45362852128`、main CI `45148041908` 和 GHCR 发布 `45148042125` 均成功。这些记录只补充提案依据，提案仍待维护人批准。

### 6.1 Node.js 24 兼容性

`main` 的 `binary-release.yml` 使用 `ubuntu-24.04` GitHub-hosted runner。发布 run `44969396475` 的 `publish` job 对 `download-artifact@v4` 报告 Node.js 20 弃用警告。`upload-artifact@v4` 在该次运行中没有单独显示弃用警告，但官方 metadata 声明它使用 Node.js 20，因此提案继续覆盖两个 action。

官方 action metadata、tag commit 和 self-hosted runner 最低版本已核实。

```text
actions/upload-artifact@v4: runs.using=node20; tag commit=ea165f8d65b6e75b540449e92b4886f43607fa02
actions/upload-artifact@v6: runs.using=node24; tag commit=b7c566a772e6b6bfb58ed0dc250532a479d7789f
actions/download-artifact@v4: runs.using=node20; tag commit=d3f86a106a0bac45b974a628896c90dbdf5c8093
actions/download-artifact@v7: runs.using=node24; tag commit=37930b1c2abaa49bbe596cd826c3c89aef350131
Node.js 24 minimum self-hosted runner=2.327.1
main workflow runner=ubuntu-24.04 (GitHub-hosted)
```

当前 `dev` 候选 workflow 已使用 `upload-artifact@v6` 和 `download-artifact@v7`。`main` 的 `scripts/stable-tree.manifest` 已显式登记 `.github/workflows/binary-release.yml`，所以 action 版本改动不需要新增路径项，但仍须通过稳定树 `--strict` 检查。当前 `main` 在 `efab6cd` 的严格稳定树检查通过，检查的是现有清单，不代表未来修改已经验证。

已核对 workflow 使用的 action 输入。upload 保留名称、文件路径、`if-no-files-found: error` 和 `retention-days: 90`。download 保留精确名称、目标路径、`github-token`、repository 和 prepare run ID。v6/v7 继续支持这些输入。v4 已采用不可变工件语义，升级本身不引入该语义变化。当前 upload 没有启用 `overwrite` 或 `include-hidden-files`，后续实现也不应顺带打开这些选项。

托管 runner 最低版本、action metadata 和工件行为说明见官方版本文档。 [upload-artifact v6](https://github.com/actions/upload-artifact/releases/tag/v6) · [download-artifact v7](https://github.com/actions/download-artifact/releases/tag/v7)

### 6.2 已发布版本依赖公告

对 `v0.1.0` 精确源码 `31d1aa4a51a56512f96c10a466cbf394f10ff5cc` 的锁文件运行 `cargo deny check advisories`，命中 `RUSTSEC-2026-0592`，受影响组件为 `url 2.5.8`。该 Release SBOM 也记录 `url 2.5.8`。当前 `main` 与 `dev` 锁定 `url 2.5.9`，同一 cargo-deny 检查通过。已发布 tag、Release 资产、签名和来源证明保持原样。

应用级检索和锁定依赖正常依赖图源码检索的范围与结果见 §6.2.1。现有检索没有证明公告 API 在 `v0.1.0` 二进制中的调用可达性或不可达性。维护人已于 2026-11-26 记录风险处置裁定（接受残余风险，见 `docs/plan/evidence/PRE-017-v010-url公告风险裁定-2026-11-26.md`），该裁定不构成可达性证明。公告原文与受影响版本范围见 [RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592)。

后续稳定版本实施前需确认锁文件与 SBOM 使用 `url >= 2.5.9`，`cargo deny check advisories` 通过，并归档对既有 `v0.1.0` 的风险处置结论。不得修改旧 tag、Release 资产、签名或 provenance。后续稳定发布仍受 PRE-013 和 G4 顺序条件约束。PRE-017 不授权创建新 tag、运行 `publish` 或推送稳定镜像。

#### 6.2.1 正常依赖图源码检索（2026-11-25）

以精确的 `v0.1.0` 源码和锁文件运行 `cargo tree --locked --offline -p tgm --edges normal --invert url@2.5.8`，确认正式二进制的正常依赖图包含 `url 2.5.8`。对该正常依赖图中可用 crate 的 `src/` Rust 源码进行文本检索，共检查 295 个含源码的包、7,732 个 Rust 文件。没有命中 `Url::origin()`、`url::Url::origin()` 或 `url::quirks::origin()` 的显式调用。

源码检索出现的 `origin()` 同名调用位于 `aws-smithy-http-client` 的 HTTP 连接池 `OriginKey`、`aws-sdk-kms` 的 KMS `OriginType` builder，以及 `sqlx-macros-core` 的 `ColumnOrigin` 元数据。检查了这些类型定义和调用位置，它们不是 `url::Url`。同一精确源码的 `cargo deny check advisories` 仍因 `RUSTSEC-2026-0592` 以退出码 1 失败；修复后的 `main`/`dev` 锁文件已使用 `url 2.5.9`，该检查通过。

该检查只覆盖锁文件正常依赖图中可用 crate 的 `src/` 文本调用。它没有证明优化构建产物中的动态可达性，也没有覆盖 `build.rs` 输出、宏展开、运行时反射或无法从源码文本识别的调用形式。因此 `v0.1.0` 的风险裁定仍待维护人记录，不能据本次结果关闭公告风险。

```text
v0.1.0_normal_binary_dependency_url=2.5.8
normal_dependency_source_packages_scanned=295
rust_source_files_scanned=7732
explicit_url_origin_call_text_hits=0
same_name_origin_calls=aws-smithy-http-client-OriginKey;aws-sdk-kms-OriginType;sqlx-macros-core-ColumnOrigin
v0.1.0_cargo_deny_advisory=RUSTSEC-2026-0592;expected-fail
cross_crate_or_binary_reachability=not-proven
maintainer_risk_disposition=pending
```

### 6.3 与稳定分支的实施边界

`main` 是独立的精简稳定源码历史，与 `dev` 没有共同祖先。PR #139 只更新了 `dev`。若本提案获批，`main` 上的 workflow 修复应从当前 `main` 基线单独建立受签名保护的 PR，并通过稳定树严格检查；不能把普通 `dev` 合并当作该修复的发布路径。后续稳定版本的 `prepare` 可验收 action 上传/下载和审批材料，不能运行 `publish`。

```text
proposal_approved=approved-2026-11-26
main_workflow_changes=merged-8c10c2c-PR-141
prepare_runtime_acceptance=prepare-pass-run-45770352988
download_verify=PR-145-ci-pass-open
published_url_2_5_8_advisory_review=disposition-recorded-2026-11-26
maintainer_risk_disposition=recorded-2026-11-26-accept-residual-risk
new_stable_tag=not-authorized
new_stable_release=not-authorized
```

## 七、实施记录（2026-11-26）

批准后按 §四 审批分界实施 §3.1：`main` 的 `binary-release.yml`
`upload-artifact@v4 → @v6`、`download-artifact@v4 → @v7`（仅两行版本
变更，输入语义不变）。以受签名提交在独立 PR #141 上从 `main` 基线
`efab6cd` 单独建立（§6.3 边界），未以 `dev` 合并充当发布路径。

验证（PR #141 合入前后）：

- 合并前：对 `8c10c2c` 与基线 `efab6cd` 各跑一次
  `scripts/check-stable-tree.py --strict`，两次均通过（437 个文件），
  证明改动后稳定树清单仍满足严格白名单；`scripts/tests` 中稳定树
  单测 12 条通过；workflow YAML 可解析。
- 合并后：`main` 上 `8c10c2c` 的 CI（static、database）成功。
- `binary-release.yml` 是 `workflow_dispatch` 触发，合入本身不产生
  发布运行；`prepare` 的 Node 24 运行时验收（§3.3 的上传/下载比对）
  要等下一次经批准的 `prepare` 运行，保持 `not-run`。

未实施部分及原因：§3.2 的 `v0.1.0` 风险裁定属维护人动作
（`maintainer_risk_disposition=pending`）；新稳定 `prepare` / `publish`
与稳定镜像推送按 §四 需要针对精确 source commit 与工件摘要的独立批准，
本提案不授权。G4 生产扫描授权仍按 G4 提案单独等待。

## 八、实施记录增量（2026-11-26）：v0.1.0 风险裁定

§四 授权范围内，维护人以责任方身份完成 §3.2 的风险处置裁定并归档
`docs/plan/evidence/PRE-017-v010-url公告风险裁定-2026-11-26.md`：
接受 `v0.1.0` 残余风险、修复随下一稳定版本交付、旧资产保持不可变。
§6.2.1 的历史快照块保持原样（那是 2026-11-25 检索时的状态记录），
当前状态以 §6.3 状态块为准。

本增量不改 `v0.1.0` 的任何已发布材料，不创建新 tag，不运行 `publish`。
新稳定版本的 `prepare` 运行时验收（§3.3 的上传/下载比对）在批准后另行记录。

## 九、实施记录增量（2026-11-26）：prepare 运行时验收运行

按 §四 授权运行了不发布 Release 的 `prepare` 验收：`workflow_dispatch`
`mode=prepare`、`version=0.1.0`、`source_commit=8c10c2c`（当时 `main` 的
tip），run `45770352988` 成功。观察结果：

- `upload-artifact@v6` 与 `download-artifact@v7` 运行日志没有 Node.js 20
  deprecation、`ubuntu-latest` migration 或依赖公告告警；
- 工件 `stable-release-approval-v0.1.0-run-45770352988` 的 `assets/` 有 7 个文件，
  `approval/` 有 2 个文件，保留期 90 天（`expires_at=2027-02-24`）；
- `APPROVAL.json` SHA-256 为
  `40b72e66ea8af14db3987c344c96353b4bce634b286437b1aa4a9d75ca89c909`，
  状态 `awaiting-explicit-release-approval`，source_commit 记为 `8c10c2c`。

本次运行使用 `0.1.0` 检查 action 行为。`publish` job 会先检查既有
Release，并要求维护人对精确 source commit 与工件摘要审批。本次运行产物
状态为未签名、未发布。

§3.3 的下载比对需要独立的 `download-artifact@v7` 运行路径，已在
`main` 基线上以受签名提交建立工作流 `stable-release-verify.yml`
（PR #145，base `main`）。该 PR 合入并执行一次按精确 run ID 的验证
运行后，`prepare_runtime_acceptance` 才能记为完整通过。

```text
[report]
task: PRE-017-stable-release-runtime-and-advisory-followup
path: ok
gates: pass
escalation: -
```
