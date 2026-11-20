#!/usr/bin/env python3
import argparse
import importlib.util
import os
import re
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
    parser.add_argument("--config", type=Path, default=Path.home() / ".config/tgm-g4/acceptance.env")
    parser.add_argument("--since", type=int, required=True, help="Unix 秒；只核对本次运行之后的测试标记")
    args = parser.parse_args()

    try:
        values = MODULE.parse_config(args.config)
        MODULE.validate_sessions(values)
    except (OSError, UnicodeError, ValueError):
        print("G4 只读标记核对：配置或 session 预检失败", file=sys.stderr)
        return 2

    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(Path.home()),
        "CARGO_TARGET_DIR": str(ROOT / "target/ai-gates-task"),
        "TGM_PLATFORM_API_ID": values["TGM_PLATFORM_API_ID"],
        "TGM_LIVE_BOT2_TOKEN": values["TGM_LIVE_BOT2_TOKEN"],
        "TGM_LIVE_TARGET_CHAT": values["TGM_LIVE_TARGET_CHAT"],
        "TGM_G4_OBSERVER_SESSION_FILE": values["TGM_G4_OBSERVER_SESSION_FILE"],
        "TGM_G4_PROBE_SINCE": str(args.since),
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
        "live_g4_readonly_marker_check",
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
        print("G4 只读标记核对超时；没有执行任何发送操作", file=sys.stderr)
        return 3
    marker = "g4_readonly_recent_messages="
    summary = next((line for line in result.stdout.splitlines() if line.startswith(marker)), None)
    stages = [
        line.split("=", 1)[1]
        for line in result.stdout.splitlines()
        if line.startswith("g4_readonly_stage=")
    ]
    failure = next(
        (
            line.split("=", 1)[1]
            for line in result.stdout.splitlines()
            if line.startswith("g4_readonly_failure=")
        ),
        None,
    )
    if result.returncode != 0 or summary is None:
        categories = (
            ("session_or_auth", r"AUTH_KEY|SESSION_REVOKED|unauthorized|session 快照|session 快照无效"),
            ("flood_wait", r"FLOOD_WAIT"),
            ("peer_or_access", r"CHANNEL_INVALID|CHANNEL_PRIVATE|PEER_ID_INVALID|无法解析 Telegram peer"),
            ("network_or_timeout", r"Connection refused|connect error|timed out|network|dns"),
            ("observer_mismatch", r"observer session 没有有效 Bot id"),
            ("test_or_build_failure", r"error\[E[0-9]+\]|could not compile|test .* panicked|FAILED"),
        )
        found = [name for name, pattern in categories if re.search(pattern, result.stdout, re.IGNORECASE)]
        detail = ",".join(found) if found else "unclassified"
        last_stage = stages[-1] if stages else "none"
        if failure is not None:
            detail = failure
        print(
            f"G4 只读标记核对失败：类别={detail}，最后阶段={last_stage}；原始客户端输出已抑制",
            file=sys.stderr,
        )
        return 3

    fields = dict(item.split("=", 1) for item in summary.split())
    matches = int(fields.get("marker_matches", "-1"))
    scanned = int(fields.get("g4_readonly_recent_messages", "-1"))
    print(f"G4 只读标记核对：扫描匹配数={scanned}，本次时间窗标记数={matches}")
    if matches != 0:
        print("发现既有测试标记；禁止重新投递，需先人工确认清理", file=sys.stderr)
        return 4
    return 0


if __name__ == "__main__":
    sys.exit(main())
