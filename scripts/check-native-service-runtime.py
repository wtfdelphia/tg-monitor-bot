#!/usr/bin/env python3
"""Check a rendered application template in an isolated user systemd unit."""

from __future__ import annotations

import argparse
import os
import shutil
import signal
import subprocess
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path


def command(*arguments: str, check: bool = True, environment: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(arguments, env=environment, text=True, capture_output=True, timeout=45)
    if check and result.returncode != 0:
        raise AssertionError(f"{arguments[0]} {arguments[1]} 返回 {result.returncode}：{result.stderr}")
    return result


@contextmanager
def private_manager():
    binary = shutil.which("systemd")
    if binary is None:
        raise SystemExit("缺少 systemd 可执行文件")
    host_environment = dict(os.environ)
    with tempfile.TemporaryDirectory(prefix="tgm-native-manager-", dir=os.environ["XDG_RUNTIME_DIR"]) as temporary:
        directory = Path(temporary)
        units = directory / "systemd/user"
        units.mkdir(parents=True)
        for name in ("default", "basic", "shutdown"):
            (units / f"{name}.target").write_text("[Unit]\nDescription=TGM isolated manager target\nDefaultDependencies=no\n", encoding="utf-8")
        for name in ("exit.target", "systemd-exit.service"):
            source = next((Path(prefix) / name for prefix in ("/usr/lib/systemd/user", "/lib/systemd/user")
                           if (Path(prefix) / name).is_file()), None)
            if source is None:
                raise SystemExit(f"缺少系统提供的用户 manager 退出工件：{name}")
            (units / name).symlink_to(source)
        name = directory.name + ".service"
        environment = os.environ | {"XDG_RUNTIME_DIR": str(directory)}
        environment.pop("DBUS_SESSION_BUS_ADDRESS", None)
        command("systemd-run", "--user", f"--unit={name}", "--property=Delegate=yes", "--property=Type=exec",
                "--property=KillMode=control-group", "--property=TimeoutStopSec=10s", "/usr/bin/env", f"XDG_RUNTIME_DIR={directory}",
                f"SYSTEMD_UNIT_PATH={units}", "SYSTEMD_LOG_TARGET=console", "/usr/bin/dbus-run-session", "--",
                binary, "--user", "--unit=default.target")
        try:
            deadline = time.monotonic() + 15
            while command("systemctl", "--user", "show-environment", check=False, environment=environment).returncode:
                if time.monotonic() >= deadline:
                    raise AssertionError("临时用户 manager 未就绪")
                time.sleep(0.1)
            yield environment
        finally:
            command("systemctl", "--user", "exit", check=False, environment=environment)
            stopped = command("systemctl", "--user", "stop", name, check=False, environment=host_environment)
            snapshot = command("systemctl", "--user", "show", name, "--property=ActiveState,MainPID",
                              check=False, environment=host_environment)
            if "MainPID=0" not in snapshot.stdout or "ActiveState=inactive" not in snapshot.stdout:
                raise AssertionError(f"临时 manager 未清理：{stopped.stderr} {snapshot.stdout}")


def check_runtime(args: argparse.Namespace, manager_environment: dict[str, str]) -> None:
    if not args.env_file.parent.name.startswith("tgm-native-test-"):
        raise SystemExit("只接受 Rust 隔离测试创建的临时环境文件")
    if "TGM_PROFILE=development" not in args.env_file.read_text(encoding="utf-8").splitlines():
        raise SystemExit("只接受 development 隔离验收配置")
    def control(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return command("systemctl", "--user", *arguments, check=check, environment=manager_environment)

    control("show-environment")
    command(str(args.binary), "serve", "--role", args.role, "--env-file", str(args.env_file), "--check-config")
    root = Path(__file__).resolve().parents[1]
    template = (root / "deploy/systemd/app/user/tgm-app@.service").read_text(encoding="utf-8")
    runtime = Path(manager_environment["XDG_RUNTIME_DIR"]) / "systemd/user"
    with tempfile.TemporaryDirectory(prefix="tgm-native-runtime-") as temporary:
        directory = Path(temporary)
        name = directory.name + "@"
        source = directory / f"{name}.service"
        instance = f"{name}{args.role}.service"
        link = runtime / source.name
        anchor = directory / f"{directory.name}.target"
        anchor_link = runtime / anchor.name
        if any(path.exists() or path.is_symlink() for path in (link, anchor_link)):
            raise SystemExit("临时 unit 名称冲突，拒绝覆盖")
        source.write_text(template.replace("%h/.local/bin/tgm", str(args.binary))
                          .replace("%h/.config/tgm/app/%i.env", str(args.env_file)), encoding="utf-8")
        anchor.write_text(f"[Unit]\nDescription=TGM isolated runtime check\nWants={instance}\n", encoding="utf-8")
        original = args.env_file.read_bytes()
        original_mode = args.env_file.stat().st_mode & 0o777

        def state() -> dict[str, str]:
            result = control("show", instance, "--property=ActiveState,SubState,Result,MainPID,NRestarts,ControlGroup,ExecMainStatus")
            return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)

        def logs(pid: int | None = None) -> str:
            process = () if pid is None else (f"_PID={pid}",)
            if manager_environment["XDG_RUNTIME_DIR"] != os.environ["XDG_RUNTIME_DIR"]:
                return command("journalctl", f"_SYSTEMD_USER_UNIT={Path(manager_environment['XDG_RUNTIME_DIR']).name}.service",
                               f"SYSLOG_IDENTIFIER=tgm-app-{args.role}", "--no-pager", "-n", "500",
                               "--output=cat", "--all", *process).stdout
            return command("journalctl", "--user", "-u", instance, "--no-pager", "-n", "500", "--output=cat", "--all", *process).stdout

        def ready(snapshot: dict[str, str]) -> bool:
            pid = int(snapshot.get("MainPID", "0"))
            if snapshot.get("SubState") != "running" or pid <= 0:
                return False
            environment = Path(f"/proc/{pid}/environ")
            if not environment.exists() or b"TGM_PROFILE=development\0" not in environment.read_bytes():
                return False
            marker = "HTTP 服务已启动" if args.role in ("control-plane", "bot-gateway") else (
                "已启动" if args.role in ("mtproto-worker", "delivery") else "subscriber 已装"
            )
            return marker in logs(pid)

        def wait(predicate, seconds: int = 35) -> dict[str, str]:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                snapshot = state()
                if predicate(snapshot):
                    return snapshot
                time.sleep(0.1)
            raise AssertionError(f"{instance} 超时；state={state()}；journal={logs()}")

        def reset_failed() -> None:
            state()
            control("reset-failed", instance)

        try:
            control("link", "--runtime", str(source))
            control("link", "--runtime", str(anchor))
            control("daemon-reload")
            parameters = control("show", instance,
                                 "--property=StartLimitIntervalUSec,StartLimitBurst,RestartUSec,Restart,KillMode,TimeoutStopUSec")
            loaded = dict(line.split("=", 1) for line in parameters.stdout.splitlines() if "=" in line)
            if loaded != {"StartLimitIntervalUSec": "2min", "StartLimitBurst": "3", "RestartUSec": "10s",
                          "Restart": "on-failure", "KillMode": "control-group", "TimeoutStopUSec": "30s"}:
                raise AssertionError(f"实际加载参数与模板不同：{loaded}")
            control("start", "--no-block", anchor.name)
            wait(ready)
            pid = int(state()["MainPID"])
            if Path(f"/proc/{pid}").stat().st_uid != os.getuid():
                raise AssertionError("实际服务身份与用户级身份不同")
            inherited = Path(f"/proc/{pid}/environ").read_bytes()
            names = {entry.split(b"=", 1)[0] for entry in inherited.split(b"\0")}
            allowed = {line.partition("=")[0].encode() for line in original.decode("utf-8").splitlines()
                       if line and not line.lstrip().startswith("#")}
            if not names - {b""} <= allowed | {b"PATH", b"HOME"}:
                raise AssertionError("服务环境包含配置白名单以外的继承项")
            for forbidden in (b"TGM_DB_URL_OWNER", b"DATABASE_URL", b"TGM_DB_URL_TENANT_PURGE", b"AWS_ACCESS_KEY_ID"):
                if forbidden in names:
                    raise AssertionError("服务继承了非角色凭据")
            cgroup = state()["ControlGroup"]
            control("stop", instance)
            wait(lambda snapshot: snapshot.get("ActiveState") == "inactive" and snapshot.get("MainPID") == "0")
            if Path(f"/proc/{pid}").exists():
                raise AssertionError("stop 后主进程仍在运行")
            group_processes = Path("/sys/fs/cgroup") / cgroup.lstrip("/") / "cgroup.procs"
            if group_processes.exists() and group_processes.read_text().strip():
                raise AssertionError("stop 后控制组还有残留进程")
            if state().get("ExecMainStatus") != "0":
                raise AssertionError("应用没有优雅退出")
            time.sleep(11)
            if state().get("ActiveState") != "inactive":
                raise AssertionError("人工停止后服务自动重启")

            if args.role == "rule":
                reset_failed()
                control("start", "--no-block", instance)
                wait(ready)
                pid = int(state()["MainPID"])
                os.kill(pid, signal.SIGKILL)
                wait(lambda snapshot: ready(snapshot) and int(snapshot["MainPID"]) != pid
                     and int(snapshot.get("NRestarts", "0")) >= 1)
                control("stop", instance)

                unavailable = original.replace(b"127.0.0.1:55432", b"127.0.0.1:1")
                if unavailable == original:
                    raise AssertionError("隔离配置没有可替换的测试数据库端口")
                args.env_file.write_bytes(unavailable)
                reset_failed()
                control("start", "--no-block", instance)
                wait(lambda snapshot: snapshot.get("SubState") == "auto-restart"
                     and snapshot.get("MainPID") == "0")
                args.env_file.write_bytes(original)
                wait(lambda snapshot: ready(snapshot) and int(snapshot.get("NRestarts", "0")) >= 1)
                control("stop", instance)

                args.env_file.unlink()
                reset_failed()
                control("start", "--no-block", instance)
                wait(lambda snapshot: snapshot.get("MainPID") == "0" and snapshot.get("Result") == "exit-code")
                control("stop", instance)
                args.env_file.write_bytes(original)
                args.env_file.chmod(0o644)
                reset_failed()
                control("start", "--no-block", instance)
                wait(lambda snapshot: snapshot.get("MainPID") == "0" and snapshot.get("Result") == "exit-code")
                control("stop", instance)
                args.env_file.chmod(original_mode)

                broken = original.replace(b"TGM_PROFILE=development", b"TGM_PROFILE=broken", 1)
                args.env_file.write_bytes(broken)
                reset_failed()
                restarts_before_config_failure = int(state().get("NRestarts", "0"))
                control("start", "--no-block", instance)
                failed = wait(
                    lambda snapshot: snapshot.get("ActiveState") == "failed"
                    and snapshot.get("MainPID") == "0"
                    and int(snapshot.get("NRestarts", "0")) >= restarts_before_config_failure + 3,
                    seconds=80,
                )
                if failed["MainPID"] != "0" or int(failed["NRestarts"]) < restarts_before_config_failure + 3:
                    raise AssertionError("连续配置失败未按模板限速")
                time.sleep(11)
                if state().get("ActiveState") != "failed" or state().get("MainPID") != "0":
                    raise AssertionError("限速后仍自动启动")
                args.env_file.write_bytes(original)
                reset_failed()
                control("start", "--no-block", instance)
                wait(ready)
                control("stop", instance)

            journal = logs()
            for line in original.decode("utf-8").splitlines():
                name_part, separator, value = line.partition("=")
                if separator and value and ("SECRET" in name_part or "TOKEN" in name_part or "KEK" in name_part or "DB_URL" in name_part):
                    if len(value) >= 8 and value in journal:
                        raise AssertionError("应用 journal 包含敏感配置值")
            print(f"{args.role}: user systemd 启停、身份、环境、journal 与残留检查通过" +
                  ("；进程失败恢复、依赖恢复、缺失/过宽配置拒绝、配置失败限速及恢复通过" if args.role == "rule" else ""))
        finally:
            control("stop", instance, check=False)
            control("stop", anchor.name, check=False)
            control("reset-failed", instance, check=False)
            args.env_file.write_bytes(original)
            args.env_file.chmod(original_mode)
            for installed, target in ((link, source), (anchor_link, anchor)):
                if installed.is_symlink() and installed.resolve() == target:
                    installed.unlink()
            control("daemon-reload", check=False)


def main() -> None:
    parser = argparse.ArgumentParser(description="仅用临时用户级 unit 验证应用启停；禁止生产配置")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--env-file", type=Path, required=True)
    parser.add_argument("--role", choices=("control-plane", "bot-gateway", "mtproto-worker", "fanout", "rule", "delivery"), required=True)
    parser.add_argument("--isolated-manager", action="store_true")
    args = parser.parse_args()
    if os.getuid() == 0:
        raise SystemExit("用户级验收不得以 root 运行")
    if args.isolated_manager:
        with private_manager() as environment:
            check_runtime(args, environment)
    else:
        check_runtime(args, dict(os.environ))


if __name__ == "__main__":
    main()
