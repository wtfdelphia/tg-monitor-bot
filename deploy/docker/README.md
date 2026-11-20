# Docker 发布包预检

目标机只需要 Docker Engine、Docker Compose v2、Bash、`sha256sum`、`grep`、`find` 等常见
Coreutils，以及本目录发布包；不需要 Rust、Cargo、源码或宿主机 PostgreSQL 客户端。发布包由
`scripts/wp8-docker-release-package.sh` 从已经构建并验收过的镜像生成。

## 目标机执行

把完整发布目录复制到目标机后，在发布目录执行：

```bash
./scripts/wp8-docker-release-preflight.sh
```

脚本按以下顺序执行：

1. 用 `package-files.sha256` 校验所有发布文件；
2. `docker load` 导入 `image.tar`，核对跨主机可复现的镜像内容指纹；
3. 核对非 root 运行用户，并确认运行时没有源码、Cargo 缓存、`.env` 或构建期环境变量；
4. 使用锁定 digest 的 `postgres:17.11-alpine3.23` 和 Docker 提供的 `/dev/shm` tmpfs 启动
   隔离数据库，并通过显式入口包装在容器内再次设置 `PGDATA` 为
   `/dev/shm/tgm-postgres-data`；不使用宿主目录 bind mount、Docker named volume 或
   overlayfs 数据目录，避免旧内核、SELinux、user namespace 和 UID 映射阻止 PostgreSQL
   写入；健康后由验收脚本通过 `psql` 执行角色初始化文件。可用
   `TGM_ACCEPTANCE_PG_IMAGE=postgres:17.11-bookworm` 临时切换到 Debian 备选镜像；
5. 通过镜像执行 `tgm migrate`、`tgm audit-rls` 和合成租户清理探针；
6. 清理本次随机命名的 Compose project、网络和一次性容器；`/dev/shm` 中的 PostgreSQL 数据
   随容器退出销毁，保留已加载镜像。

预检不会连接生产数据库、Redis、对象存储、Telegram、KMS 或真实备份，不启用生产调度，也不
删除真实租户。脚本失败时停止后续破坏性探针；不得使用 `docker system prune --volumes`。

## 生成发布包

构建端先生成并验收镜像，再在干净且已提交的工作树中执行：

```bash
scripts/wp8-docker-release-package.sh \
  --image tgm-wp8-release:<commit> \
  --output /path/to/tgm-wp8-release-package
```

发布目录包含镜像归档、镜像 ID（诊断用）、镜像内容指纹、来源提交、逐文件 SHA-256、隔离
Compose 工件、角色初始化文件和预检脚本。发布包不包含源码、Cargo 缓存、生产环境文件、生产数据库密码、Telegram
凭据、KMS Secret、session 或二维码；隔离验收工件中的固定密码只用于合成非生产 PostgreSQL。

默认 PostgreSQL 验收镜像为 `postgres:17.11-alpine3.23@sha256:05aa77262db9277fde6374bb2f901c9604b8a9373029e1a7993440c6aae48736`。
该 digest 用于规避旧版宿主 `libseccomp` 对 Alpine 3.24 的兼容性问题；生产运行时镜像不受此验收配置改变。

发布包预检通过只表示目标机 Docker 运行工件与合成数据清理链路可复现，不表示生产备份恢复、
RPO/RTO、跨副本留存、生产告警、生产探针或周期调度已经通过。
