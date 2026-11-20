#!/usr/bin/env python3
"""Keep a local synthetic staging stack and exercise backup, restore and local alerts."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import grp
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import shlex
import shutil
import stat
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_STATE = Path.home() / ".local/state/tgm-prestage-20261116"
DOCKER_SOCKET = Path("/var/run/docker.sock")
POSTGRES = "postgres:17.11@sha256:f4c66b820c6f974249089d3d16d86a3698eae11e8746eb6644b2271031e91232"
REDIS = "redis:8-alpine@sha256:ef216d85ec44f4fdf7c0e6df2d1757a4734ed32fbb278cabd3181b5a27353b79"
MINIO = "ghcr.io/coollabsio/minio:RELEASE.2025-10-15T17-29-55Z@sha256:69b55a1c1c5dc285ce04db96689f5b2102317fc77a50680a1874ca6efd1c87f9"
RUNTIME_PATHS = ["Cargo.toml", "Cargo.lock", ".cargo", ".sqlx", "rust-toolchain.toml",
                 "bins/tgm/src", "bins/tgm/Cargo.toml", "migrations", "deploy/native", "deploy/systemd"]
for crate in ("core", "db", "tg", "app"):
    RUNTIME_PATHS.extend([f"crates/{crate}/src", f"crates/{crate}/Cargo.toml"])
COMPOSE = """services:
  postgres:
    image: POSTGRES_IMAGE
    environment:
      POSTGRES_PASSWORD: ${PG_PASSWORD}
      POSTGRES_DB: tgm
    ports: ["127.0.0.1::5432"]
    volumes: ["postgres-data:/var/lib/postgresql/data"]
    networks: [isolated]
    healthcheck:
      test: ["CMD-SHELL", "psql -U postgres -d tgm -c 'SELECT 1' >/dev/null"]
      interval: 2s
      timeout: 3s
      retries: 30
    restart: unless-stopped
  redis:
    image: REDIS_IMAGE
    environment:
      REDIS_PASSWORD: ${REDIS_PASSWORD}
    command: ["/bin/sh", "-ec", 'exec redis-server --appendonly yes --requirepass "$$REDIS_PASSWORD"']
    ports: ["127.0.0.1::6379"]
    volumes: ["redis-data:/data"]
    networks: [isolated]
    healthcheck:
      test: ["CMD-SHELL", 'test "$$(REDISCLI_AUTH="$$REDIS_PASSWORD" redis-cli ping)" = PONG']
      interval: 2s
      timeout: 3s
      retries: 30
    restart: unless-stopped
  minio:
    image: MINIO_IMAGE
    environment:
      MINIO_ROOT_USER: ${MINIO_USER}
      MINIO_ROOT_PASSWORD: ${MINIO_PASSWORD}
    command: server /data
    ports: ["127.0.0.1::9000"]
    volumes: ["minio-data:/data"]
    networks: [isolated]
    restart: unless-stopped
networks:
  isolated:
    internal: true
volumes:
  postgres-data:
  redis-data:
  minio-data:
""".replace("POSTGRES_IMAGE", POSTGRES).replace("REDIS_IMAGE", REDIS).replace("MINIO_IMAGE", MINIO)


def write_private(path, text):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        stream.write(text)
    path.chmod(0o600)


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def clean_environment():
    allowed = ("PATH", "HOME", "DOCKER_CONFIG", "XDG_RUNTIME_DIR")
    return {key: os.environ[key] for key in allowed if key in os.environ} | {
        "DOCKER_HOST": "unix:///var/run/docker.sock", "LC_ALL": "C.UTF-8",
    }


class Stage:
    def __init__(self, state):
        self.state = state.expanduser().absolute()
        self.config_path = self.state / "state.json"
        if self.state.is_symlink():
            raise RuntimeError("拒绝符号链接状态目录")
        if self.state.exists():
            metadata = self.state.stat()
            if metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
                raise RuntimeError("状态目录必须属于当前用户且权限为 0700")
        if self.config_path.exists() or self.config_path.is_symlink():
            metadata = self.config_path.lstat()
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                    or metadata.st_mode & 0o077):
                raise RuntimeError("状态文件必须是当前用户拥有的 0600 普通文件")
        self.config = json.loads(self.config_path.read_text()) if self.config_path.exists() else None
        if self.config and not re.fullmatch(r"tgm-prestage-[0-9a-f]{10}", self.config.get("project", "")):
            raise RuntimeError("项目标识不属于本脚本")
        self.environment = clean_environment()

    @contextmanager
    def lock(self):
        self.state.mkdir(parents=True, mode=0o700, exist_ok=True)
        descriptor = os.open(self.state / "operation.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise RuntimeError("本预发布栈已有操作运行，请稍后重试") from error
            self.__init__(self.state)
            yield
        finally:
            os.close(descriptor)

    def candidate(self):
        candidate = Path(self.config["candidate"])
        for relative, expected in self.config["candidate_files"].items():
            if hashlib.sha256((candidate / relative).read_bytes()).hexdigest() != expected:
                raise RuntimeError("候选载荷在初始化后发生变化")
        return candidate

    def run(self, arguments, data=None, check=True, timeout=180):
        result = subprocess.run(arguments, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=self.environment, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"{Path(arguments[0]).name} 失败 rc={result.returncode}；诊断不回显凭据")
        return result

    def compose(self, *arguments, check=True):
        if not self.config:
            raise RuntimeError("先运行 up")
        return self.run(["docker", "compose", "--project-name", self.config["project"],
                         "--env-file", str(self.state / "stack.env"),
                         "--file", str(self.state / "compose.yaml"), *arguments], check=check)

    def save(self):
        write_private(self.config_path, json.dumps(self.config, indent=2) + "\n")

    def event(self, event, **fields):
        descriptor = os.open(self.state / "events.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            stream.write(json.dumps({"time": utc_now(), "event": event, **fields}) + "\n")

    def container(self, service):
        identifier = self.compose("ps", "--all", "--quiet", service).stdout.decode().strip()
        if not re.fullmatch(r"[0-9a-f]{12,64}", identifier):
            raise RuntimeError("服务容器标识不唯一或不存在")
        return identifier

    def sql(self, container, query, user="postgres", database="tgm", check=True):
        return self.run(["docker", "exec", "-i", container, "psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1",
                         "-U", user, "-d", database], query.encode(), check=check)

    def binary(self, container, command, database="tgm"):
        result = self.run(["docker", "exec", "--user", "postgres", container, "env", "-i",
                           "PATH=/usr/local/bin:/usr/bin:/bin", "TGM_PROFILE=development",
                           f"TGM_DB_URL_OWNER=postgres://postgres@localhost/{database}?host=/var/run/postgresql",
                           "/tmp/tgm-prestage", command])
        self.event(command, result="pass", database=database)
        return result

    def initialize(self, candidate):
        if self.config:
            raise RuntimeError("状态目录已经存在；使用 status，不覆盖现有栈")
        candidate = candidate.expanduser().resolve(strict=True)
        info = json.loads((candidate / "BUILDINFO.json").read_text())
        if info.get("channel") != "candidate" or info.get("stable") is not False:
            raise RuntimeError("只接受已验证安装的非稳定候选目录")
        manifest = (candidate / "SHA256SUMS").read_text().splitlines()
        for relative in ("bin/tgm", "BUILDINFO.json"):
            expected = [line.split()[0] for line in manifest if line.split()[-1] == relative]
            if expected != [hashlib.sha256((candidate / relative).read_bytes()).hexdigest()]:
                raise RuntimeError("候选安装目录与包内摘要不一致")
        self.run(["git", "-C", str(ROOT), "diff", "--quiet", info["source_commit"], "HEAD", "--", *RUNTIME_PATHS])
        for image in (POSTGRES, REDIS, MINIO):
            self.run(["docker", "image", "inspect", image])
        self.state.mkdir(parents=True, mode=0o700, exist_ok=True)
        self.config = {
            "project": "tgm-prestage-" + secrets.token_hex(5), "created_at": utc_now(),
            "candidate": str(candidate), "source_commit": info["source_commit"],
            "checkout_commit": self.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"]).stdout.decode().strip(),
            "runtime_sources_unchanged": True, "backup_failures": 0,
            "candidate_files": {relative: hashlib.sha256((candidate / relative).read_bytes()).hexdigest()
                                for relative in ("bin/tgm", "BUILDINFO.json")},
            "applications": "not-started-oidc-proposal-and-inputs-pending",
        }
        self.save()
        write_private(self.state / "compose.yaml", COMPOSE)
        write_private(self.state / "stack.env", "\n".join(f"{key}={secrets.token_hex(24)}" for key in (
            "PG_PASSWORD", "REDIS_PASSWORD", "MINIO_USER", "MINIO_PASSWORD")) + "\n")
        write_private(self.state / "restore.env", "POSTGRES_PASSWORD=" + secrets.token_hex(24) + "\n")
        self.compose("up", "--detach", "--wait", "--pull", "never")
        container = self.container("postgres")
        self.run(["docker", "cp", str(candidate / "bin/tgm"), container + ":/tmp/tgm-prestage"])
        self.run(["docker", "exec", container, "chmod", "0555", "/tmp/tgm-prestage"])
        self.binary(container, "migrate")
        self.binary(container, "audit-rls")
        query = """
        WITH created AS (
          INSERT INTO tenants(name, owner_user_id) VALUES ('prestage-a',900000001),('prestage-b',900000002)
          RETURNING id, owner_user_id
        ), members AS (
          INSERT INTO tenant_members(tenant_id,user_id) SELECT id,owner_user_id FROM created
        ) INSERT INTO keywords(tenant_id,word) SELECT id,'prestage-' || id::text FROM created;
        """
        self.sql(container, query)
        self.config["initialized"] = True
        self.save()
        self.event("stack.created", data="synthetic-only", external_services="none")
        self.status()

    def status(self):
        services = {}
        for service in ("postgres", "redis", "minio"):
            container = self.container(service)
            metadata = json.loads(self.run(["docker", "inspect", container]).stdout)[0]
            if metadata["Config"]["Labels"].get("com.docker.compose.project") != self.config["project"]:
                raise RuntimeError("容器归属与本次预发布项目不一致")
            bindings = metadata["NetworkSettings"]["Ports"]
            services[service] = {"running": metadata["State"]["Running"], "ports": bindings}
            for ports in bindings.values():
                for binding in ports or []:
                    if binding["HostIp"] != "127.0.0.1":
                        raise RuntimeError("发现非 loopback 发布端口")
            for network in metadata["NetworkSettings"]["Networks"]:
                network_info = json.loads(self.run(["docker", "network", "inspect", network]).stdout)[0]
                if not network_info["Internal"]:
                    raise RuntimeError("依赖容器接入非 internal 网络")
        print(json.dumps({"project": self.config["project"], "services": services,
                          "applications": self.config["applications"],
                          "backup": {key: self.config["backup"][key] for key in ("completed_at", "sha256")}
                          if self.config.get("backup") else "not-run",
                          "restore": self.config.get("restore", "not-run"),
                          "schedule": self.config.get("schedule", "not-run"),
                          "alert_sink": "local-events-file-only; external-receipt-not-run",
                          "production": "untouched"}, ensure_ascii=False, indent=2))

    def backup(self, fault=False):
        try:
            self.write_backup(fault)
        except (RuntimeError, OSError, subprocess.SubprocessError):
            temporary = self.state / "backups/pending.dump"
            temporary.unlink(missing_ok=True)
            self.config["backup_failures"] += 1
            self.event("backup.failed", consecutive=self.config["backup_failures"], synthetic_fault=fault)
            if self.config["backup_failures"] >= 2:
                self.event("alert.backup_failed_twice", receiver="local-file", delivery="external-not-run")
            self.save()
            if not fault:
                raise

    def write_backup(self, fault):
        container = self.container("postgres")
        directory = self.state / "backups"
        directory.mkdir(mode=0o700, exist_ok=True)
        temporary = directory / "pending.dump"
        started = time.time()
        database = "prestage_missing_database_fault" if fault else "tgm"
        with temporary.open("wb") as stream:
            result = subprocess.run(["docker", "exec", container, "pg_dump", "-U", "postgres", "-Fc", "-d", database],
                                    env=self.environment, stdout=stream, stderr=subprocess.PIPE, timeout=120)
        temporary.chmod(0o600)
        if result.returncode:
            raise RuntimeError("备份失败；保留上次成功备份，已记录本地事件")
        if fault:
            temporary.unlink()
            raise AssertionError("故障数据库意外存在；演练拒绝通过")
        self.run(["docker", "exec", "-i", container, "pg_restore", "--list"], temporary.read_bytes())
        roles = self.run(["docker", "exec", container, "pg_dumpall", "-U", "postgres", "--roles-only", "--no-role-passwords"]).stdout.decode()
        roles = "\n".join(line for line in roles.splitlines()
                          if line != "CREATE ROLE postgres;" and not line.startswith("ALTER ROLE postgres ")) + "\n"
        snapshot = self.snapshot(container)
        final = directory / (str(time.time_ns()) + ".dump")
        temporary.rename(final)
        write_private(final.with_suffix(".roles.sql"), roles)
        self.config["backup"] = {
            "path": str(final), "sha256": hashlib.sha256(final.read_bytes()).hexdigest(),
            "roles_sha256": hashlib.sha256(roles.encode()).hexdigest(), "snapshot": snapshot,
            "started_epoch": started, "completed_at": utc_now(),
        }
        self.config["backup_failures"] = 0
        self.save()
        self.event("backup.succeeded", sha256=self.config["backup"]["sha256"])
        retained = sorted(directory.glob("*.dump"), key=lambda path: path.stat().st_mtime, reverse=True)
        for expired in retained[3:]:
            expired.unlink()
            expired.with_suffix(".roles.sql").unlink(missing_ok=True)
        print("backup=pass")

    def snapshot(self, container, database="tgm"):
        tables = self.sql(container, "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename", database=database).stdout.decode().splitlines()
        if not tables or any(not re.fullmatch(r"[a-z_][a-z_0-9]*", table) for table in tables):
            raise RuntimeError("表清单为空或无效")
        counts = ",".join(f"'{table}',(SELECT count(*) FROM public.{table})" for table in tables)
        query = "SELECT jsonb_build_object('tables',jsonb_build_object(" + counts + "),'migrations'," + (
            "(SELECT jsonb_object_agg(version::text,encode(checksum,'hex')) FROM _sqlx_migrations WHERE success))")
        return json.loads(self.sql(container, query, database=database).stdout)

    def verify_restore(self, container):
        database = "tgm_restore"
        self.binary(container, "audit-rls", database)
        if self.snapshot(container, database) != self.config["backup"]["snapshot"]:
            raise RuntimeError("恢复后的全表计数或迁移摘要不一致")
        tenants = self.sql(container, "SELECT id FROM tenants ORDER BY id", database=database).stdout.decode().splitlines()
        if len(tenants) != 2 or any(not tenant.isdigit() for tenant in tenants):
            raise RuntimeError("合成租户夹具不一致")
        for tenant in tenants:
            query = (f"BEGIN; SET LOCAL app.tenant_id='{tenant}'; SELECT count(*) || ':' || "
                     f"count(*) FILTER (WHERE tenant_id<>{tenant}) FROM keywords; ROLLBACK;")
            if self.sql(container, query, user="app_user", database=database).stdout.strip() != b"1:0":
                raise RuntimeError("受限角色双租户隔离检查失败")
        result = self.sql(container, "SELECT secret_ciphertext FROM identity_secrets", user="tenant_purge", database=database, check=False)
        if result.returncode == 0 or b"permission denied" not in result.stderr:
            raise RuntimeError("清理角色凭据列拒绝检查失败")
        flags = self.sql(container, "SELECT bool_and(NOT rolsuper AND NOT rolbypassrls) FROM pg_roles WHERE rolname IN ('app_user','tenant_purge')", database=database)
        if flags.stdout.strip() != b"t":
            raise RuntimeError("运行角色边界错误")

    def restore(self):
        backup = self.config.get("backup")
        if not backup:
            raise RuntimeError("先运行 backup")
        archive = Path(backup["path"])
        roles = archive.with_suffix(".roles.sql")
        if hashlib.sha256(archive.read_bytes()).hexdigest() != backup["sha256"] or hashlib.sha256(roles.read_bytes()).hexdigest() != backup["roles_sha256"]:
            raise RuntimeError("备份或角色清单摘要不一致")
        name = self.config["project"] + "-restore-" + secrets.token_hex(4)
        started = time.monotonic()
        restored = None
        self.run(["docker", "run", "--detach", "--rm", "--name", name, "--network", "none",
                  "--label", "tgm.prestage.owner=" + self.config["project"],
                  "--tmpfs", "/var/lib/postgresql/data:rw,size=512m", "--env-file", str(self.state / "restore.env"), POSTGRES])
        try:
            deadline = time.monotonic() + 60
            while self.sql(name, "SELECT 1", database="postgres", check=False).returncode:
                if time.monotonic() >= deadline:
                    raise RuntimeError("恢复实例启动超时")
                time.sleep(1)
            self.sql(name, roles.read_text(), database="postgres")
            self.run(["docker", "exec", name, "createdb", "-U", "postgres", "-T", "template0", "tgm_restore"])
            self.run(["docker", "exec", "-i", name, "pg_restore", "--exit-on-error", "-U", "postgres", "-d", "tgm_restore"], archive.read_bytes())
            candidate = self.candidate()
            self.run(["docker", "cp", str(candidate / "bin/tgm"), name + ":/tmp/tgm-prestage"])
            self.run(["docker", "exec", name, "chmod", "0555", "/tmp/tgm-prestage"])
            self.verify_restore(name)
            restored = {
                "result": "pass", "completed_at": utc_now(),
                "rto_seconds": round(time.monotonic() - started, 3),
                "backup_age_at_restore_seconds": round(time.time() - backup["started_epoch"], 3),
                "network": "none", "data": "synthetic-only", "production_rpo_rto": "not-established",
                "checks": ["audit-rls", "migration-checksums", "all-table-counts", "restricted-two-tenant-reads", "credential-column-denied"],
            }
        finally:
            self.run(["docker", "stop", "--time", "5", name])
            deadline = time.monotonic() + 10
            while self.run(["docker", "inspect", name], check=False).returncode == 0:
                if time.monotonic() >= deadline:
                    raise RuntimeError("恢复容器未清理")
                time.sleep(0.2)
        self.config["restore"] = restored
        self.save()
        self.event("restore.succeeded", **restored)
        print(json.dumps(restored, ensure_ascii=False, indent=2))

    def fault_check(self):
        if not self.config.get("backup"):
            raise RuntimeError("故障演练前先留一份成功备份")
        previous = self.config["backup"]["sha256"]
        self.backup(fault=True)
        self.backup(fault=True)
        if self.config["backup"]["sha256"] != previous or self.config["backup_failures"] < 2:
            raise RuntimeError("备份失败保护或连续失败计数错误")
        container = self.container("redis")
        self.compose("stop", "redis")
        try:
            running = self.run(["docker", "inspect", "--format", "{{.State.Running}}", container]).stdout.strip()
            if running != b"false":
                raise RuntimeError("Redis 故障注入未生效")
            self.event("alert.dependency_unavailable", service="redis", receiver="local-file", delivery="external-not-run")
        finally:
            self.compose("up", "--detach", "--wait", "--pull", "never", "redis")
        self.backup()
        self.event("dependency.recovered", service="redis")
        print("fault_check=pass; external_alert_receipt=not-run")

    def schedule(self, install):
        directory = Path.home() / ".config/systemd/user"
        name = self.config["project"] + "-backup"
        if not install:
            self.run(["systemctl", "--user", "disable", "--now", name + ".timer"])
            for suffix in (".timer", ".service"):
                path = directory / (name + suffix)
                expected = self.config.get("schedule_files", {}).get(suffix)
                if path.exists():
                    if expected != hashlib.sha256(path.read_bytes()).hexdigest():
                        raise RuntimeError("本任务 unit 被修改，保留文件供人工确认")
                    path.unlink()
            self.run(["systemctl", "--user", "daemon-reload"])
            self.config["schedule"] = "disabled"
            self.save()
            return
        self.run(["systemctl", "--user", "show-environment"])
        directory.mkdir(parents=True, exist_ok=True)
        group_id = DOCKER_SOCKET.stat().st_gid
        account = pwd.getpwuid(os.getuid())
        if group_id not in os.getgrouplist(account.pw_name, account.pw_gid):
            raise RuntimeError("当前账号未获 Docker socket 组权限，不修改账号或 socket")
        group_name = grp.getgrgid(group_id).gr_name
        launcher = shutil.which("sg", path=os.defpath)
        if not launcher or not re.fullmatch(r"[a-z_][a-z_0-9-]*", group_name):
            raise RuntimeError("缺少可用的现有 Docker 组启动方式")
        runner = self.state / "backup-runner.py"
        write_private(runner, Path(__file__).read_text())
        command = "exec " + shlex.join((sys.executable, str(runner), "backup",
                                         "--state-dir", str(self.state)))
        quoted = []
        for argument in (launcher, group_name, "-c", command):
            if any(character in argument for character in ('\n', '\r')):
                raise RuntimeError("unit 路径包含不支持字符")
            quoted.append('"' + argument.replace('\\', '\\\\').replace('"', '\\"')
                          .replace('%', '%%').replace('$', '$$') + '"')
        service = ("[Unit]\nDescription=TGM synthetic prestage backup\n[Service]\nType=oneshot\n"
                   "UMask=0077\nTimeoutStartSec=180\nExecStart=" + " ".join(quoted) + "\n")
        timer = ("[Unit]\nDescription=TGM synthetic prestage hourly backup\n[Timer]\n"
                 "OnActiveSec=1h\nOnUnitActiveSec=1h\nUnit=" + name + ".service\n"
                 "[Install]\nWantedBy=timers.target\n")
        self.config["schedule_files"] = {}
        for suffix, text in ((".service", service), (".timer", timer)):
            path = directory / (name + suffix)
            if path.exists() and path.read_text() != text:
                raise RuntimeError("unit 已存在且内容不同，拒绝覆盖")
            write_private(path, text)
            self.config["schedule_files"][suffix] = hashlib.sha256(path.read_bytes()).hexdigest()
        self.save()
        self.run(["systemctl", "--user", "daemon-reload"])
        self.backup()
        self.run(["systemctl", "--user", "enable", "--now", name + ".timer"])
        self.run(["systemctl", "--user", "is-active", "--quiet", name + ".timer"])
        self.config["schedule"] = "user-timer-hourly-latest-three-synthetic-backups"
        self.save()
        print("schedule=pass; user_linger=unchanged; external_alert_receipt=not-run")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("up", "status", "backup", "restore", "fault-check", "schedule", "unschedule", "stop", "down"))
    parser.add_argument("--state-dir", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--candidate-dir", type=Path)
    parser.add_argument("--discard-data", action="store_true")
    args = parser.parse_args()
    if os.getuid() == 0 or os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock") != "unix:///var/run/docker.sock":
        raise RuntimeError("仅允许普通用户操作本机 Docker socket")
    stage = Stage(args.state_dir)
    if args.command != "up" and not stage.config:
        raise RuntimeError("先运行 up")
    with stage.lock():
        return execute(args, stage)


def execute(args, stage):
    if args.command == "up":
        if stage.config and stage.config.get("initialized"):
            stage.compose("up", "--detach", "--wait", "--pull", "never")
            stage.status()
        elif args.candidate_dir is None:
            parser.error("up 需要 --candidate-dir，先用候选安装器验证并隔离安装")
        else:
            stage.initialize(args.candidate_dir)
    elif args.command == "status":
        stage.status()
    elif args.command == "backup":
        stage.backup()
    elif args.command == "restore":
        stage.restore()
    elif args.command == "fault-check":
        stage.fault_check()
    elif args.command in ("schedule", "unschedule"):
        stage.schedule(args.command == "schedule")
    elif args.command == "stop":
        if stage.config.get("schedule_files"):
            stage.schedule(False)
        stage.compose("stop")
    else:
        if stage.config.get("schedule_files"):
            stage.schedule(False)
        stage.compose("down", "--remove-orphans", *(["--volumes"] if args.discard_data else []))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError):
        print("预发布操作失败；保留状态及上次成功备份，未输出命令凭据", file=sys.stderr)
        raise SystemExit(1)
