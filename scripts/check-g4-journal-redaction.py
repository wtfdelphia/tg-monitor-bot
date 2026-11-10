#!/usr/bin/env python3
import argparse
import json
import os
import re
import stat
import subprocess
import sys
from pathlib import Path


SECRET_KEYS = (
    "TGM_PLATFORM_API_HASH",
    "TGM_LIVE_BOT_TOKEN",
    "TGM_LIVE_BOT2_TOKEN",
)
SESSION_KEYS = (
    "TGM_G4_SENDER_SESSION_FILE",
    "TGM_G4_OBSERVER_SESSION_FILE",
)
REQUIRED_KEYS = (
    "TGM_G4_FLOOD_WAIT_ALLOW",
    "TGM_PLATFORM_API_ID",
    *SECRET_KEYS,
    "TGM_LIVE_TARGET_CHAT",
    *SESSION_KEYS,
)
ALLOWED_KEYS = frozenset((*REQUIRED_KEYS, "TGM_G4_FLOOD_WAIT_MAX_SECONDS"))
VALUE_PATTERNS = {
    "TGM_G4_FLOOD_WAIT_ALLOW": re.compile(r"1"),
    "TGM_PLATFORM_API_ID": re.compile(r"[0-9]+"),
    "TGM_PLATFORM_API_HASH": re.compile(r"[A-Fa-f0-9]{32}"),
    "TGM_LIVE_BOT_TOKEN": re.compile(r"[0-9]+:[A-Za-z0-9_-]+"),
    "TGM_LIVE_BOT2_TOKEN": re.compile(r"[0-9]+:[A-Za-z0-9_-]+"),
    "TGM_LIVE_TARGET_CHAT": re.compile(r"-?[0-9]+"),
    "TGM_G4_SENDER_SESSION_FILE": re.compile(r"/[A-Za-z0-9_./-]+"),
    "TGM_G4_OBSERVER_SESSION_FILE": re.compile(r"/[A-Za-z0-9_./-]+"),
    "TGM_G4_FLOOD_WAIT_MAX_SECONDS": re.compile(r"[1-9][0-9]*"),
}


def regular_owned_file(path: Path, mode: int) -> bytes:
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("not an owned regular file")
    if stat.S_IMODE(metadata.st_mode) != mode:
        raise ValueError("unexpected file mode")
    return path.read_bytes()


def parse_config(path: Path) -> dict[str, str]:
    metadata = path.parent.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("config directory is not owned")
    if stat.S_IMODE(metadata.st_mode) != 0o700:
        raise ValueError("config directory mode must be 700")
    raw = regular_owned_file(path, 0o600).decode("utf-8")
    values: dict[str, str] = {}
    for line in raw.splitlines():
        if not line or line.startswith("#"):
            continue
        name, separator, value = line.partition("=")
        if not separator or not name.replace("_", "A").isalnum() or not name.isupper():
            raise ValueError("invalid config line")
        if name not in ALLOWED_KEYS or VALUE_PATTERNS[name].fullmatch(value) is None:
            raise ValueError("unsupported config key or value format")
        if name in values:
            raise ValueError("duplicate config key")
        values[name] = value
    missing = [name for name in REQUIRED_KEYS if not values.get(name)]
    if missing:
        raise ValueError("required config keys missing")
    if not 0 < int(values["TGM_PLATFORM_API_ID"]) <= 2_147_483_647:
        raise ValueError("invalid API ID range")
    if not -(2**63) <= int(values["TGM_LIVE_TARGET_CHAT"]) < 2**63:
        raise ValueError("invalid target chat range")
    maximum = values.get("TGM_G4_FLOOD_WAIT_MAX_SECONDS", "1800")
    if not 0 < int(maximum) <= 2**64 - 1:
        raise ValueError("invalid maximum wait range")
    return values


def validate_sessions(values: dict[str, str]) -> None:
    session_dir: Path | None = None
    for name in SESSION_KEYS:
        path = Path(values[name])
        if not path.is_absolute():
            raise ValueError("session path must be absolute")
        if session_dir is None:
            session_dir = path.parent
        elif path.parent != session_dir:
            raise ValueError("session files must share a directory")
        regular_owned_file(path, 0o600)
    assert session_dir is not None
    metadata = session_dir.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("session directory is not owned")
    if stat.S_IMODE(metadata.st_mode) != 0o700:
        raise ValueError("session directory mode must be 700")


def report_redaction(path: Path, needles: list[bytes]) -> tuple[bytes, bool]:
    report = regular_owned_file(path, 0o600)
    redacted = not any(needle and needle in report for needle in needles)
    return report, redacted


def main() -> int:
    parser = argparse.ArgumentParser()
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--preflight", action="store_true")
    action.add_argument("--finalize", action="store_true")
    action.add_argument("--capture", action="store_true")
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--journal", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--unit")
    parser.add_argument("--since")
    args = parser.parse_args()

    try:
        values = parse_config(args.config)
        validate_sessions(values)
        if args.preflight:
            print("G4 配置与 session 元数据：pass")
            return 0
        if args.capture:
            if args.journal is None or args.unit is None or args.since is None:
                raise ValueError("journal path, unit, and start time are required")
            secrets = [values[name].encode() for name in ("TGM_PLATFORM_API_ID", *SECRET_KEYS)]
            secrets.extend(Path(values[name]).read_bytes() for name in SESSION_KEYS)
            if any(not secret for secret in secrets):
                raise ValueError("configuration or session secret is empty")
            output_mode = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
            descriptor = os.open(args.journal, output_mode, 0o600)
            count = 0
            redacted = True
            command = subprocess.Popen(
                [
                    "journalctl",
                    "--user",
                    "--unit",
                    args.unit,
                    "--since",
                    args.since,
                    "--no-pager",
                    "--output=json",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            try:
                assert command.stdout is not None
                with os.fdopen(descriptor, "wb") as output:
                    for raw in command.stdout:
                        try:
                            entry = json.loads(raw)
                        except (json.JSONDecodeError, UnicodeDecodeError):
                            continue
                        if entry.get("_SYSTEMD_USER_UNIT") != args.unit:
                            continue
                        count += 1
                        if any(secret in raw for secret in secrets):
                            redacted = False
                            continue
                        message = entry.get("MESSAGE", "")
                        if isinstance(message, str):
                            output.write(message.encode("utf-8", errors="replace") + b"\n")
                    output.flush()
                    os.fsync(output.fileno())
            finally:
                if command.stdout is not None:
                    command.stdout.close()
            if command.wait() != 0 or count == 0:
                raise ValueError("journal is unavailable or has no entries for the G4 unit")
            if not redacted:
                raise ValueError("journal contains configured secret material")
            print("G4 journal capture: pass")
            return 0
        if args.journal is None or args.report is None:
            raise ValueError("journal and report paths are required")

        journal = regular_owned_file(args.journal, 0o600)
        if not journal:
            raise ValueError("journal is empty")
        needles = [values[name].encode() for name in ("TGM_PLATFORM_API_ID", *SECRET_KEYS)]
        needles.extend(Path(values[name]).read_bytes() for name in SESSION_KEYS)
        if any(not session for session in needles[len(SECRET_KEYS) + 1 :]):
            raise ValueError("session file is empty")
        report, report_ok = report_redaction(args.report, needles)
        journal_ok = not any(needle and needle in journal for needle in needles)
        passed = journal_ok and report_ok
        lines = report.decode("utf-8").splitlines()
        matches = [index for index, line in enumerate(lines) if line.startswith("journal_redaction:")]
        if len(matches) != 1:
            raise ValueError("report journal_redaction field is missing or duplicated")
        lines[matches[0]] = f"journal_redaction: {'pass' if passed else 'fail'}"
        report_path = args.report
        descriptor = os.open(report_path, os.O_WRONLY | os.O_TRUNC | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "wb") as output:
            output.write(("\n".join(lines) + "\n").encode())
        print(f"G4 journal_redaction: {'pass' if passed else 'fail'}")
        return 0 if passed else 1
    except (OSError, UnicodeError, ValueError) as error:
        if args.finalize and args.report is not None:
            try:
                report, _ = report_redaction(args.report, [])
                lines = report.decode("utf-8").splitlines()
                matches = [index for index, line in enumerate(lines) if line.startswith("journal_redaction:")]
                if len(matches) == 1:
                    lines[matches[0]] = "journal_redaction: fail"
                    descriptor = os.open(args.report, os.O_WRONLY | os.O_TRUNC | os.O_NOFOLLOW)
                    with os.fdopen(descriptor, "wb") as output:
                        output.write(("\n".join(lines) + "\n").encode())
            except (OSError, UnicodeError, ValueError):
                pass
        print(f"G4 journal_redaction: fail ({type(error).__name__})")
        return 1


if __name__ == "__main__":
    sys.exit(main())
