# TGM 原生二进制发布包

版本：`@VERSION@`

来源提交：`@SOURCE_COMMIT@`

目标：`@TARGET@`

## 发布包内容

- `bin/tgm`：已在 glibc 2.17 兼容构建器中生成的单二进制程序。
- `BUILDINFO.json`：来源提交、目标、工具链和构建器摘要。
- `VERSION`：安装器使用的版本标识。
- `SHA256SUMS`：包内文件校验值。
- `sbom.cdx.json`：构建输入 SBOM。
- `deploy/systemd/`：已批准的 cleanup unit、timer 和环境变量示例。
- `scripts/install-native.sh`：需要显式选择 root 或 user 模式的离线安装器。
- `scripts/native-release-preflight.sh`：安装前执行的动态依赖、glibc、systemd unit/timer 和帮助命令预检。

## 安装前验证

从 GitHub Release 同时取得以下三个文件：

```text
tgm-@VERSION@-linux-x86_64-gnu.tar.gz
SHA256SUMS
SHA256SUMS.sigstore.json
```

目标机必须是 Linux x86-64，glibc 不低于 2.17。安装器要求本机已有 `cosign`、`tar`、
`sha256sum`、`ldd`、`openssl` 和 `systemd-analyze`；它不会联网下载工具、依赖或脚本。签名验证失败、
压缩包校验失败、包内校验失败、动态依赖不满足、unit/timer 预检失败或帮助命令失败时，
安装器拒绝切换当前版本。

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

## cleanup 范围

本首片只打包仓库现有的 cleanup unit/timer。应用角色原生 systemd service、生产 timer
启用、迁移、真实租户物理删除和生产凭据配置均不在安装器职责内。
