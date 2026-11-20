#!/usr/bin/env python3
"""Run the signed candidate's six app roles against disposable local services."""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional


ROLES = ("control-plane", "bot-gateway", "mtproto-worker", "fanout", "rule", "delivery")
SOURCE_COMMIT = "eb4f5aaf72e35885883423460b0038f5c6e9649e"
COMPOSE = """services:
  postgres:
    image: postgres:17.11-alpine3.23@sha256:05aa77262db9277fde6374bb2f901c9604b8a9373029e1a7993440c6aae48736
    environment:
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: pw
      POSTGRES_DB: tgm
      PGDATA: /dev/shm/tgm-postgres-data
    entrypoint: ["/bin/sh", "-ec", "export PGDATA=/dev/shm/tgm-postgres-data; exec docker-entrypoint.sh postgres"]
    shm_size: 256m
    ports: ["127.0.0.1:${PG_PORT}:5432"]
    healthcheck:
      test: ["CMD-SHELL", "psql -U postgres -d tgm -c 'SELECT 1' || exit 1"]
      interval: 2s
      timeout: 3s
      retries: 30
  redis:
    image: redis:8-alpine
    ports: ["127.0.0.1:${REDIS_PORT}:6379"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 2s
      timeout: 3s
      retries: 30
  minio:
    image: ghcr.io/coollabsio/minio:RELEASE.2025-10-15T17-29-55Z@sha256:69b55a1c1c5dc285ce04db96689f5b2102317fc77a50680a1874ca6efd1c87f9
    command: server /data
    ports: ["127.0.0.1:${MINIO_PORT}:9000"]
    healthcheck:
      test: ["CMD-SHELL", "mc ready local || exit 1"]
      interval: 2s
      timeout: 3s
      retries: 30
"""


def redact(text: str) -> str:
    text = re.sub(r"(?:postgres|redis)://[^\s\"']+", "<redacted-url>", text)
    return re.sub(
        r"(?i)(secret|token|password|api_hash|local_kek|access_key_id|secret_access_key|client_secret)([=: ]+)[^\s\"']+",
        r"\1\2<redacted>",
        text,
    )


def run(*arguments: str, environment: dict[str, str], input_path: Optional[Path] = None,
        capture: bool = True, timeout: int = 150) -> subprocess.CompletedProcess[str]:
    with input_path.open("rb") if input_path else open(os.devnull, "rb") as input_file:
        result = subprocess.run(arguments, env=environment, stdin=input_file, text=True,
                                stdout=subprocess.PIPE if capture else None,
                                stderr=subprocess.STDOUT if capture else None, timeout=timeout)
    if result.returncode and capture:
        raise RuntimeError(f"命令失败 {arguments[0]} {arguments[1:3]}：{redact(result.stdout or '')}")
    if result.returncode:
        raise RuntimeError(f"命令失败 {arguments[0]} {arguments[1:3]} rc={result.returncode}")
    return result


def port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def environment_for(role: str, database: str, postgres_port: int, redis_port: int, minio_port: int, platform_id: int) -> str:
    values = {
        "TGM_PROFILE": "development", "RUST_LOG": "info,sqlx=warn",
        "TGM_DB_URL_APP": f"postgres://app_user:apw@127.0.0.1:{postgres_port}/{database}?application_name=tgm-rocky-{role}",
        "TGM_REDIS_URL": f"redis://127.0.0.1:{redis_port}",
        "TGM_DB_URL_AUTH": f"postgres://auth_lookup:lpw@127.0.0.1:{postgres_port}/{database}",
        "TGM_DB_URL_EXECUTION": f"postgres://app_user:apw@127.0.0.1:{postgres_port}/{database}",
        "TGM_DB_URL_OPS": f"postgres://platform_ops:ppw@127.0.0.1:{postgres_port}/{database}",
        "TGM_DB_URL_MEDIA_CLEANUP": f"postgres://media_cleanup:mcpw@127.0.0.1:{postgres_port}/{database}",
        "TGM_KEK_BACKEND": "env", "TGM_LOCAL_KEK": "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
        "TGM_S3_ENDPOINT": f"http://127.0.0.1:{minio_port}", "TGM_S3_BUCKET": "tgm-wp8-app",
        "TGM_S3_ACCESS_KEY_ID": "minioadmin", "TGM_S3_SECRET_ACCESS_KEY": "minioadmin",
        "TGM_PLATFORM_API_ID": "1", "TGM_PLATFORM_API_HASH": "0" * 32,
        "TGM_OIDC_BOT_ID": "1", "TGM_OIDC_ALG": "RS256",
        "TGM_OIDC_REDIRECT_URL": "http://127.0.0.1/callback",
        "TGM_OIDC_CLIENT_SECRET": "isolated-not-a-credential",
        "TGM_CONTROL_BOT_TOKEN": "1:isolated-not-a-credential",
        "TGM_CONTROL_BOT_SECRET_TOKEN": "isolated-not-a-credential",
        "TGM_CONTROL_BOT_IDENTITY_ID": str(platform_id), "TGM_CONTROL_BOT_USERNAME": "isolated_test_bot",
        "TGM_DATA_BOT_SECRET_TOKEN": "isolated-not-a-credential", "TGM_DATA_BOT_IDENTITY_ID": str(platform_id),
        "TGM_LISTEN_ADDR": "127.0.0.1:0", "TGM_BOT_GATEWAY_LISTEN_ADDR": "127.0.0.1:0",
    }
    template = (ROOT / "deploy/systemd/app" / f"{role}.env.example").read_text(encoding="utf-8")
    names = [line.split("=", 1)[0] for line in template.splitlines() if line and not line.startswith("#")]
    selected = [name for name in names if name in values and name != "TGM_KEK_BACKEND"]
    if role in ("control-plane", "bot-gateway", "mtproto-worker", "delivery"):
        selected.extend(["TGM_KEK_BACKEND", "TGM_LOCAL_KEK"])
    missing = [name for name in selected if name not in values]
    if missing:
        raise RuntimeError(f"{role} 缺少合成配置生成器：{','.join(missing)}")
    return "".join(f"{name}={values[name]}\n" for name in selected)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--buildinfo", type=Path)
    parser.add_argument("--source-commit", default=SOURCE_COMMIT)
    parser.add_argument("--roles", nargs="+", choices=ROLES, default=list(ROLES))
    parser.add_argument(
        "--isolated-manager",
        action="store_true",
        help="为每个角色启动临时 user manager；默认复用当前 systemctl --user manager",
    )
    parser.add_argument(
        "--system-journal",
        action="store_true",
        help="通过 sudo -n 只读检查本次临时 unit 的 system journal",
    )
    args = parser.parse_args()
    if os.getuid() == 0:
        raise SystemExit("拒绝 root：只用普通用户、临时 user unit 和本地 Docker")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        raise SystemExit("来源提交必须是完整小写 SHA")
    if os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock") != "unix:///var/run/docker.sock":
        raise SystemExit("拒绝非本机 Docker socket")
    for command in ("docker", "systemctl", "systemd-analyze", "journalctl"):
        if shutil.which(command) is None:
            raise SystemExit(f"缺少命令：{command}")
    if args.system_journal:
        if shutil.which("sudo") is None:
            raise SystemExit("--system-journal 缺少 sudo")
        journal_probe = subprocess.run(
            ("sudo", "-n", shutil.which("journalctl"), "--no-pager", "-n", "0", "--output=cat"),
            env=os.environ,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if journal_probe.returncode != 0:
            raise SystemExit("--system-journal 需要 sudoers 允许非交互只读执行 journalctl")
    if args.isolated_manager:
        for command in ("systemd-run", "dbus-run-session"):
            if shutil.which(command) is None:
                raise SystemExit(f"--isolated-manager 缺少命令：{command}")
    if subprocess.run(("systemctl", "--user", "show-environment"),
                      env=os.environ, stdin=subprocess.DEVNULL,
                      stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                      text=True, check=False).returncode != 0:
        raise SystemExit("当前用户没有可用的 systemctl --user manager")
    if args.binary is None:
        candidates = []
        for version_file in (Path.home() / ".local/opt/tgm/candidates").glob("*/VERSION"):
            try:
                buildinfo = json.loads((version_file.parent / "BUILDINFO.json").read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if (buildinfo.get("channel") == "candidate" and buildinfo.get("stable") is False
                    and buildinfo.get("source_commit") == args.source_commit
                    and version_file.read_text(encoding="utf-8").strip() == buildinfo.get("version")):
                candidates.append((version_file.parent / "bin/tgm", buildinfo))
        if len(candidates) != 1:
            raise SystemExit("需要唯一已验证的候选安装目录；先运行候选安装器或显式提供 --binary 与 --buildinfo")
        args.binary, candidate_buildinfo = candidates[0]
    else:
        if args.buildinfo is None:
            raise SystemExit("直接传入 --binary 时必须同时传入 --buildinfo")
        candidate_buildinfo = json.loads(args.buildinfo.read_text(encoding="utf-8"))
    if candidate_buildinfo.get("channel") != "candidate" or candidate_buildinfo.get("stable") is not False:
        raise SystemExit("BUILDINFO 不是非稳定候选")
    if candidate_buildinfo.get("source_commit") != args.source_commit:
        raise SystemExit("BUILDINFO 来源提交不匹配")
    version = candidate_buildinfo.get("version", "")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+-candidate\.[1-9][0-9]*\.[1-9][0-9]*\.[0-9a-f]{12}", version):
        raise SystemExit("候选版本格式无效")
    if not version.endswith(args.source_commit[:12]):
        raise SystemExit("候选版本来源后缀不匹配")
    binary = args.binary.resolve(strict=True)
    if binary.name != "tgm":
        raise SystemExit("候选二进制必须命名为 tgm")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit("候选二进制不存在或不可执行")
    environment = {key: value for key, value in os.environ.items() if key in {
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "SSL_CERT_FILE"
    }}
    print(f"候选版本：{version}")
    if not environment.get("XDG_RUNTIME_DIR") or not Path(environment["XDG_RUNTIME_DIR"]).is_dir():
        raise SystemExit("需要普通用户可用的 XDG_RUNTIME_DIR 与 user systemd")
    project = f"tgm-wp8-app-{os.getpid()}-{secrets.token_hex(4)}"
    with tempfile.TemporaryDirectory(prefix="tgm-wp8-rocky-app-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o700)
        compose = directory / "compose.yaml"
        compose.write_text(COMPOSE, encoding="utf-8")
        ports = {"PG_PORT": str(port()), "REDIS_PORT": str(port()), "MINIO_PORT": str(port())}
        compose_environment = environment | ports
        def compose_command(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
            return run("docker", "compose", "-p", project, "-f", str(compose), *arguments,
                       environment=compose_environment)
        failed = True
        try:
            print(f"候选来源：{args.source_commit}；角色：{','.join(args.roles)}")
            print("启动本机临时 PostgreSQL / Redis / MinIO；无生产凭据、无 Telegram 外呼")
            with socket.socket() as probe:
                if probe.connect_ex(("127.0.0.1", int(ports["PG_PORT"]))) == 0:
                    raise RuntimeError("随机 PostgreSQL 端口已被其他进程占用")
            compose_command("up", "-d", "--wait")
            run("docker", "compose", "-p", project, "-f", str(compose), "exec", "-T", "postgres",
                "psql", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", "tgm",
                environment=compose_environment, input_path=ROOT / "scripts/init/01-roles.sql")
            owner_environment = environment | {"TGM_DB_URL_OWNER": f"postgres://postgres:pw@127.0.0.1:{ports['PG_PORT']}/tgm"}
            run(str(binary), "migrate", environment=owner_environment)
            run(str(binary), "audit-rls", environment=owner_environment)
            platform_id = int(compose_command(
                "exec", "-T", "postgres", "psql", "-At", "-U", "postgres", "-d", "tgm", "-c",
                "INSERT INTO identities (kind,tenant_id,status) VALUES ('bot',NULL,'offline') RETURNING id").stdout.splitlines()[0])
            for role in args.roles:
                role_directory = directory / f"tgm-native-test-{role}"
                role_directory.mkdir(mode=0o700)
                env_file = role_directory / "role.env"
                env_file.write_text(environment_for(role, "tgm", int(ports["PG_PORT"]), int(ports["REDIS_PORT"]),
                                                    int(ports["MINIO_PORT"]), platform_id), encoding="utf-8")
                env_file.chmod(0o600)
                print(f"== {role}：临时 user unit 全生命周期检查 ==", flush=True)
                checker_arguments = [
                    sys.executable, str(ROOT / "scripts/check-native-service-runtime.py"),
                    "--binary", str(binary), "--role", role, "--env-file", str(env_file),
                ]
                if args.isolated_manager:
                    checker_arguments.append("--isolated-manager")
                if args.system_journal:
                    checker_arguments.append("--system-journal")
                result = run(*checker_arguments, environment=environment, capture=True, timeout=600)
                print(redact(result.stdout or ""), end="", flush=True)
            failed = False
            scope = "六角色" if set(args.roles) == set(ROLES) else f"指定角色（{','.join(args.roles)}）"
            print(f"{scope}隔离运行时验收通过；未启用清理 timer、未删除真实数据、未安装生产服务。")
            print("系统级清理角色、生产留存和跨副本删除不在本片。")
        finally:
            if failed:
                diagnostics = subprocess.run(
                    ("docker", "compose", "-p", project, "-f", str(compose),
                     "logs", "--no-color", "--timestamps"),
                    env=compose_environment,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=120,
                    check=False,
                )
                if diagnostics.stdout:
                    print("--- 临时 Compose 脱敏诊断 ---", file=sys.stderr)
                    print(redact(diagnostics.stdout), file=sys.stderr, end="")
            print(f"清理临时 Compose project：{project}", flush=True)
            try:
                subprocess.run(("docker", "compose", "-p", project, "-f", str(compose),
                                "down", "-v", "--remove-orphans"), env=compose_environment,
                               stdin=subprocess.DEVNULL, timeout=300, check=True)
            except (RuntimeError, subprocess.SubprocessError) as error:
                print(f"临时 Compose 清理失败；project={project}，compose={compose}；{redact(str(error))}", file=sys.stderr)
                print("仅允许人工核对该项目名后清理；不得删除其他 Compose 栈。", file=sys.stderr)
                raise
    return 0


ROOT = Path(__file__).resolve().parents[1]
if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f"验收失败：{redact(str(error))}", file=sys.stderr)
        raise SystemExit(1)
