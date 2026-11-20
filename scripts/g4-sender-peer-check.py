#!/usr/bin/env python3
import argparse
import importlib.util
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts/check-g4-journal-redaction.py"
SPEC = importlib.util.spec_from_file_location("g4_journal_redaction", CHECKER)
if SPEC is None or SPEC.loader is None:
    raise SystemExit("无法加载 G4 配置检查器")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--config",
        type=Path,
        default=Path.home() / ".config/tgm-g4/acceptance.env",
    )
    args = parser.parse_args()

    try:
        values = MODULE.parse_config(args.config)
        MODULE.validate_sessions(values)
    except (OSError, UnicodeError, ValueError):
        print("G4 sender peer 只读检查：配置或 session 预检失败", file=sys.stderr)
        return 2

    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(Path.home()),
        "CARGO_TARGET_DIR": str(ROOT / "target/ai-gates-task"),
        "TGM_PLATFORM_API_ID": values["TGM_PLATFORM_API_ID"],
        "TGM_LIVE_BOT_TOKEN": values["TGM_LIVE_BOT_TOKEN"],
        "TGM_LIVE_TARGET_CHAT": values["TGM_LIVE_TARGET_CHAT"],
        "TGM_G4_SENDER_SESSION_FILE": values["TGM_G4_SENDER_SESSION_FILE"],
    }
    for name in ("CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"):
        if name in os.environ:
            environment[name] = os.environ[name]

    command = [
        "cargo",
        "test",
        "-p",
        "tgm-tg",
        "--lib",
        "live_g4_sender_peer_resolution_check",
        "--",
        "--ignored",
        "--nocapture",
    ]
    try:
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            text=True,
            timeout=120,
        )
    except subprocess.TimeoutExpired:
        print("G4 sender peer 只读检查超时；没有发送消息", file=sys.stderr)
        return 3

    marker = "g4_sender_peer_resolution="
    outcome = next(
        (line.split("=", 1)[1] for line in result.stdout.splitlines() if line.startswith(marker)),
        None,
    )
    cache_marker = "g4_sender_peer_cache="
    cache_state = next(
        (
            line.split("=", 1)[1]
            for line in result.stdout.splitlines()
            if line.startswith(cache_marker)
        ),
        None,
    )
    if result.returncode != 0 or outcome is None:
        print("G4 sender peer 只读检查失败；原始客户端输出已抑制", file=sys.stderr)
        return 3
    if cache_state not in {"hit", "miss"}:
        print("G4 sender peer 只读检查失败：session 缓存状态缺失", file=sys.stderr)
        return 3
    print(f"G4 sender peer 只读检查：cache={cache_state}，resolve={outcome}")
    return 0 if outcome == "pass" else 4


if __name__ == "__main__":
    sys.exit(main())
