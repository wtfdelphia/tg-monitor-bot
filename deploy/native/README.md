# TGM 原生二进制发布包

版本：`@VERSION@`

发布通道：`@CHANNEL@`

@STABILITY_NOTICE@

来源提交：`@SOURCE_COMMIT@`

目标：`@TARGET@`

## 发布包内容

- `bin/tgm`：已在 glibc 2.17 兼容构建器中生成的单二进制程序。
- `BUILDINFO.json`：来源提交、目标、工具链和构建器摘要。
- `VERSION`：安装器使用的版本标识。
- `SHA256SUMS`：包内文件校验值。
- `sbom.cdx.json`：构建输入 SBOM。
- `deploy/systemd/`：cleanup unit、timer 和环境变量示例；`app/` 提供六角色的 system/user 模板及独立配置示例。
- `scripts/install-native.sh`：需要显式选择 root 或 user 模式的离线安装器。
- `scripts/native-release-preflight.sh`：安装前执行的动态依赖、glibc、systemd unit/timer 和帮助命令预检。

## 安装前验证

从 GitHub Release 同时取得以下三个文件：

```text
tgm-@VERSION@-linux-x86_64-gnu.tar.gz
SHA256SUMS
SHA256SUMS.sigstore.json
```

@CANDIDATE_DISTRIBUTION@

目标机必须是 Linux x86-64，glibc 不低于 2.17。安装器要求本机已有 `cosign`、`tar`、
`sha256sum`、`ldd`、`openssl` 和 `systemd-analyze`；它不会联网下载工具、依赖或脚本。签名验证失败、
压缩包校验失败、包内校验失败、动态依赖不满足、unit/timer 预检失败或帮助命令失败时，
安装器拒绝切换当前版本。

候选来源验证需要 Python 3.6 或更高版本；安装验证器不要求替换系统 Python，构建和 CI
仍使用各自既有工具链。glibc 兼容只证明二进制 ABI 的最低要求，不证明宿主 systemd 的
指令与安全能力满足部署要求。

预检输出 systemd 版本及编译特征，RPM 环境还输出 systemd 包版本。现有 unit 包含
`RestrictAddressFamilies`，因此要求 systemd 明确声明 `+SECCOMP`；`-SECCOMP` 或能力
信息缺失时拒绝。cleanup 的 `ProtectSystem=strict` 等配置如果被目标解析器忽略，即使
`verify` 返回零也拒绝。不会自动删掉配置或把 `strict` 改成 `full` 来继续安装。

支持 `systemd-analyze calendar` 时保留独立日历检查；不支持时由本机 `verify` 解析
timer，并核对解析诊断。缺少或为空的 `OnCalendar`、非法日历、未知或忽略的本任务
unit/timer 配置都会失败。上述检查不安装或启用任何服务，也不代替目标机实际沙箱验证。

## 统一目标机验收入口

候选 Release 的下载、验签、来源证明、候选隔离安装和平台分流可由仓库中的统一脚本完成：

```bash
bash scripts/tgm-native-target-acceptance.sh auto \
  --tag candidate-<run>-a<attempt>-<commit12>
```

`auto` 根据 `/etc/os-release` 识别 Rocky 9 或 CentOS 7；也可显式使用 `rocky` 或 `centos7`。
脚本内置当前候选 tag，也可通过 `--tag` 或 `TGM_CANDIDATE_TAG` 覆盖；切换候选时应显式指定，避免误验收过期候选。材料目录默认保留在
`$HOME/tgm-candidate.XXXXXX`，
便于将验收证据单独归档；也可用 `--materials <目录> --no-download` 完全离线复用已下载材料。

例如：

```bash
bash scripts/tgm-native-target-acceptance.sh auto \
  --tag candidate-42399939250-a1-eb973d62f24d \
  --helpers-dir "$PWD"
```

默认下载不会覆盖材料目录中已有的文件；需要只补齐缺失文件时使用
`--materials <目录> --reuse-materials`，完全离线复用现有材料时使用
`--materials <目录> --no-download`。两种复用方式都要求材料是当前用户拥有的非空普通文件，
不能是符号链接，且最终仍会执行完整的签名、provenance、SHA-256 和包内容验证。

下载默认按 Release 直链、GitHub API 资产端点、已登录 `gh` 顺序回退，并对每个文件执行有限轮次
重试。可用 `--download-source direct|api|gh` 固定来源，或用
`TGM_DOWNLOAD_ATTEMPTS`、`TGM_DOWNLOAD_BACKOFF`、`TGM_CONNECT_TIMEOUT` 调整重试参数。
脚本沿用 `https_proxy`/`HTTPS_PROXY` 等代理变量；这只影响目标机下载，不会自动配置 Docker
daemon。Rocky 六角色验收拉取依赖镜像时，Docker daemon 需要单独配置代理。

Rocky 模式在候选隔离安装后执行 `native-release-preflight.sh --app-services`，并调用来源一致的
`scripts/wp8-rocky-app-acceptance.py` 完成六角色非生产 user systemd 验收。若要把日志脱敏计为
通过，目标机必须临时允许 `sudo -n journalctl` 只读查询；没有该权限时脚本应失败，不会把日志检查
伪装成通过。CentOS 7 模式要求原生应用预检按 `systemd 219 -SECCOMP` 能力缺失拒绝，拒绝结果
是预期的安全结论，不得删除 `RestrictAddressFamilies` 或放宽预检。

仓库脚本本身只负责下载和验证候选材料；Rocky 六角色辅助脚本及 `scripts/init/01-roles.sql`
需从同一来源 `dev` 单独提供。可在已核对的 `dev` 工作树中生成辅助包：

```bash
git archive --format=tar.gz --output=wp8-target-acceptance-helpers.tar.gz dev \
  scripts/tgm-native-target-acceptance.sh \
  scripts/wp8-rocky-app-acceptance.py \
  scripts/check-native-service-runtime.py \
  scripts/init/01-roles.sql deploy/systemd/app
sha256sum wp8-target-acceptance-helpers.tar.gz
```

目标机解压后，在辅助包根目录执行：

```bash
bash scripts/tgm-native-target-acceptance.sh auto \
  --tag candidate-<run>-a<attempt>-<commit12> \
  --helpers-dir "$PWD"
```

如果只想补下材料目录中缺失的文件，使用 `--materials <目录> --reuse-materials`；如果完全离线
复用已下载材料，使用 `--materials <目录> --no-download`。如果切换到新的候选 Release，
使用 `--tag candidate-<run>-a<attempt>-<commit12>`。不要手工拼接归档名或版本号，脚本会从
`candidate-BUILDINFO.json` 派生并核对归档名、候选 tag、来源提交与 provenance。

脚本不会连接生产 Telegram、KMS 或对象存储，也不会启用 cleanup timer、执行生产迁移或删除真实
生产数据。Rocky 模式中的迁移和 RLS 审计仅运行在脚本创建的本机临时 PostgreSQL 中；完成后只清理
本次随机命名的 Compose project、容器、网络、卷、临时 unit 与验收目录。

## root 安装

root 模式安装到 `/opt/tgm/releases/@VERSION@`，并原子更新 `/usr/local/bin/tgm`。它还会
安装 cleanup unit、timer 和 `.env.example` 文件，但不会执行 `tgm migrate`，不会启动或
启用 systemd 服务，也不会创建生产 Secret：

```bash
bash scripts/install-native.sh --root \
  --archive tgm-@VERSION@-linux-x86_64-gnu.tar.gz \
  --checksums SHA256SUMS \
  --bundle SHA256SUMS.sigstore.json
```

安装完成后，人工执行 `systemctl daemon-reload`、备份恢复库迁移、`tgm audit-rls` 和
目标机 unit 验收；确认批准前不要启用任何 timer。

@CANDIDATE_INSTALL@

## 非 root 安装

user 模式只安装到 `$HOME/.local/opt/tgm/releases/@VERSION@`，并原子更新
`$HOME/.local/bin/tgm`。它不安装系统 unit，不改变系统服务状态，也不能访问 root-only
的 `/etc/tgm` 配置：

```bash
bash scripts/install-native.sh --user \
  --archive tgm-@VERSION@-linux-x86_64-gnu.tar.gz \
  --checksums SHA256SUMS \
  --bundle SHA256SUMS.sigstore.json
```

## 回滚

安装器保留旧版本目录。回滚只切换二进制 symlink，不回滚数据库迁移；迁移已经前滚后，
旧二进制是否兼容必须按恢复流程判断：

```bash
bash scripts/install-native.sh --root --rollback <已有版本>
bash scripts/install-native.sh --user --rollback <已有版本>
```

如果已经安装应用模板，升级和回滚前还会检查待切换版本的应用预检入口、`--env-file`、`--check-config`
和模板一致性；PRE-008 之前的二进制不能直接配合新模板回滚。安装器不重启运行中的进程，
也不替换既有 unit 或实际配置。即使仅安装二进制，也不能绕过已安装模板的兼容性检查。
配置与 schema 的旧版兼容性仍须操作者单独确认。

## 应用角色安装

`--app-services` 是显式选择：只安装应用模板，不安装 cleanup unit/timer。默认安装方式
保持 PRE-007 的行为。角色为 `control-plane`、`bot-gateway`、`mtproto-worker`、`fanout`、
`rule`、`delivery`，分别运行 `tgm-app@<role>.service`。

系统级安装由 root 执行，但每个应用进程使用独立非 root 用户和同名主组 `tgm-<role>`。
已有账号必须禁止登录、密码锁定、不附带其他组、不共享 UID/主组，且其
`/nonexistent/tgm-<role>` HOME 不存在。安装器不授予 sudo 或额外权限；既有 sudoers、ACL
和外部 IAM 策略仍须主机管理员审查，账号检查不代表这些外部权限已验收。

```bash
bash scripts/install-native.sh --root --app-services \
  --archive tgm-@VERSION@-linux-x86_64-gnu.tar.gz \
  --checksums SHA256SUMS \
  --bundle SHA256SUMS.sigstore.json
```

普通用户方式要求本机的用户级 systemd manager 可用，且 `$HOME` 与 passwd 中的 HOME
一致。六个角色同 UID 运行，**不提供角色间的操作系统凭据隔离**，必须显式接受此限制：

```bash
bash scripts/install-native.sh --user --app-services --accept-same-uid \
  --archive tgm-@VERSION@-linux-x86_64-gnu.tar.gz \
  --checksums SHA256SUMS \
  --bundle SHA256SUMS.sigstore.json
```

此模式只写用户目录，不调用 sudo，不设置 linger。用户 manager 不可用时拒绝安装应用服务；
可以不带 `--app-services` 仅安装二进制，然后以前台非 root 进程运行，或由管理员选择系统级安装。
用户模式不保证退出登录后仍运行，也不能视为系统级隔离的等价生产配置。

安装器拒绝不同内容的既有模板/示例、符号链接目标和不可信可写父目录，不改变既有目录权限。
复制新工件采用不覆盖方式，全部安装步骤成功才切换当前二进制链接。输出逐项列出本次新建的
版本目录、账号、组和工件；失败后这些内容可能保留。不得自动删除账号、既有 unit、配置或历史版本，
须按本次输出逐项复核。
安装器不创建实际 `.env`，不读取调用者 Secret，不执行迁移、审计、daemon-reload、服务启停或 enable。

## 应用配置与预检

系统配置路径为 `/etc/tgm/app/<role>.env`，建议 root 所有、组为 `tgm-<role>`、权限 `0640`；
用户配置路径为 `$HOME/.config/tgm/app/<role>.env`，必须由该用户所有、权限 `0600`。
可先将对应 `.env.example` 以最终权限复制到该路径，再由 Secret 管理流程替换占位符；
示例本身无法通过检查。不要将真实值写入发布包、Git、shell 历史或验收日志。

配置格式是 UTF-8 的 `NAME=value`：空行和整行 `#` 注释被跳过，值按原文读取；不接受
`export`、重复变量、NUL，不解释引号、不做变量展开、不执行 `$()` 或 shell 命令。
文件限制为 64 KiB，路径须绝对且没有 `..`；文件及父目录不能是符号链接，父目录不能由
不可信身份写入。root 所有的 sticky 临时目录可用于隔离验收，非 sticky 的组/公共可写目录拒绝。

白名单直接复用 `tgm-core::config` 和 `eng/01`，未修改任何角色允许的凭据集合。多配项即使
为空也拒绝，包括 owner、cleanup、`DATABASE_URL`、`PATH`、`LD_PRELOAD` 和未知变量。
应用模板先用 `ExecStartPre` 离线检查，随后再启动同一角色。继承环境被清空，只传已校验配置、
固定 PATH 和不存在的角色 HOME，防止默认读取用户 `.aws/`；AWS 配置、token 文件以及
`credential_process` 的可执行文件和证书路径应使用绝对路径，不依赖登录 shell 或 `~`。

```bash
# 系统级，root 以实际服务身份仅做离线检查
runuser -u tgm-rule -- /usr/bin/env -i /usr/local/bin/tgm serve \
  --role rule --env-file /etc/tgm/app/rule.env --check-config

# 普通用户，仅做离线检查
/usr/bin/env -i "$HOME/.local/bin/tgm" serve --role rule \
  --env-file "$HOME/.config/tgm/app/rule.env" --check-config

bash scripts/native-release-preflight.sh --binary bin/tgm \
  --unit-dir deploy/systemd --app-services
```

离线检查覆盖文件边界、角色必填/多配、KEK 选择、ARN、OIDC 算法、数据库连接格式、监听地址
和身份编号。它不连接数据库、Telegram、KMS、Redis 或 S3，不证明凭据有效、桶权限正确或
schema 已迁移；正式启动仍执行原有数据库与 KEK 自测。错误信息不打印配置值。

## 应用运行与移除

以下操作由操作者在**已批准的隔离验收环境**执行，不是安装器自动动作。生产配置、迁移和
逐角色启用仍须另行批准。先完成依赖与 schema 的独立验收，再为每个角色检查配置。

```bash
systemctl daemon-reload
systemctl start tgm-app@rule.service
systemctl status tgm-app@rule.service
journalctl -u tgm-app@rule.service --no-pager
systemctl stop tgm-app@rule.service

systemctl --user daemon-reload
systemctl --user start tgm-app@rule.service
systemctl --user status tgm-app@rule.service
journalctl --user -u tgm-app@rule.service --no-pager
systemctl --user stop tgm-app@rule.service
```

服务使用前台 `Type=simple`，SIGTERM 停止，`KillMode=control-group`，停止上限 `30s`。
失败后间隔 `10s` 重启，`StartLimitBurst=3` 限制 `120s` 内的自动重启次数；连续失败时最多进行
四次启动尝试（首次启动加三次重启），随后在限速窗口内拒绝自动启动。
人工 stop 不触发自动重启。修复配置后先离线检查，再用对应模式的 `reset-failed` 和 `start`。
更新/回滚切换链接后，旧进程不会自动切换，由操作者逐角色 `restart`。

移除时先由操作者停止并禁用自己已启用的六个实例，再只移除**本次安装且确认未被其他任务
改写**的模板链接/文件并 daemon-reload。实际配置、历史版本和系统账号不自动删除；
其他 cleanup 工件及同用户的其他服务不动。安装器不提供一键递归卸载或删用户命令。

系统模板使用 `ProtectSystem=full`、`ProtectHome`、私有临时目录/设备及无 capabilities，
用户模板不伪装具有相同的 namespace 隔离。unit 以 systemd 219 语法为兼容目标；目标
CentOS 7 仍需采集实际 systemd 版本并验证解析、账号、沙箱、六角色启动/停止、限速和日志。
`verify` 对 cleanup 和应用模板的未知/不支持/忽略配置即使返回零也拒绝；其他宿主 unit 的警告保留显示，
不冒充本模板错误。应用模式不运行 cleanup timer 的 `calendar` 检查，不能据此宣称旧
systemd 已验收 cleanup 调度。开发机通过和 glibc 兼容构建都不能替代目标机运行证据。

## 非生产回归

本仓的配置、模板与安装/归档反证随 workspace 测试运行。六角色启动测试使用本地隔离测试库和
受限数据库角色，不调用真实 Telegram 或 KMS。默认不触碰宿主 systemd manager：

```bash
export DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control
SQLX_OFFLINE=true cargo test -p tgm --test native_systemd
```

已批准的开发机用户级启停演练可显式启用；要求用户 manager、journal 与 `systemd-run` 可用。
以下模式创建并清理临时独立 user manager，避免其他任务的 `daemon-reload` 干扰限速；不启用
任何正式应用 unit，不调用 sudo。它只证明本机 systemd 的行为，不能代替 CentOS 7 实机验收：

```bash
TGM_NATIVE_SYSTEMD_TESTS=1 TGM_NATIVE_SYSTEMD_ISOLATED_MANAGER=1 SQLX_OFFLINE=true \
  cargo test -p tgm --test native_systemd \
  native_roles_start_with_restricted_credentials_and_stop_cleanly -- --nocapture
```

## cleanup 范围

cleanup 工件沿用 PRE-007；应用模板由 PRE-008 补充，两类采用独立安装选择。生产 timer
启用、数据库迁移、真实租户物理删除和生产凭据配置均不在安装器职责内。
