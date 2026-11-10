import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-g4-journal-redaction.py"
WRAPPER = Path(__file__).resolve().parents[1] / "g4-live-flood-wait.sh"
SPEC = importlib.util.spec_from_file_location("g4_journal_redaction", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class G4JournalRedactionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.config_dir = self.root / "config"
        self.config_dir.mkdir(mode=0o700)
        self.session_dir = self.root / "sessions"
        self.session_dir.mkdir(mode=0o700)
        self.sender = self.session_dir / "sender.session"
        self.observer = self.session_dir / "observer.session"
        self.sender.write_bytes(b"sender-session-fixture")
        self.observer.write_bytes(b"observer-session-fixture")
        self.sender.chmod(0o600)
        self.observer.chmod(0o600)
        self.config = self.config_dir / "acceptance.env"
        self.report = self.root / "report"
        self.journal = self.root / "journal"
        self.values = {
            "TGM_G4_FLOOD_WAIT_ALLOW": "1",
            "TGM_PLATFORM_API_ID": "12345",
            "TGM_PLATFORM_API_HASH": "a" * 32,
            "TGM_LIVE_BOT_TOKEN": "123456:SenderToken_1234567890",
            "TGM_LIVE_BOT2_TOKEN": "654321:ObserverToken_1234567890",
            "TGM_LIVE_TARGET_CHAT": "-1001234567890",
            "TGM_G4_SENDER_SESSION_FILE": str(self.sender),
            "TGM_G4_OBSERVER_SESSION_FILE": str(self.observer),
            "TGM_G4_FLOOD_WAIT_MAX_SECONDS": "1800",
        }
        self.write_config()
        self.config.chmod(0o600)
        self.report.write_text("journal_redaction: not-run\ncleanup: partial\n")
        self.report.chmod(0o600)
        self.journal.write_bytes(b"safe journal entry\n")
        self.journal.chmod(0o600)

    def tearDown(self):
        self.temporary.cleanup()

    def write_config(self):
        body = "".join(f"{key}={value}\n" for key, value in self.values.items())
        self.config.write_text(body)

    def finalize(self):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--finalize", "--config", str(self.config),
             "--journal", str(self.journal), "--report", str(self.report)],
            capture_output=True,
            check=False,
            text=True,
        )

    def test_clean_journal_sets_pass_without_printing_secrets(self):
        result = self.finalize()
        self.assertEqual(result.returncode, 0)
        self.assertEqual(self.report.read_text().splitlines()[0], "journal_redaction: pass")
        for secret in self.values.values():
            self.assertNotIn(secret, result.stdout + result.stderr)

    def test_journal_containing_token_sets_fail(self):
        self.journal.write_text(f"leaked={self.values['TGM_LIVE_BOT_TOKEN']}\n")
        result = self.finalize()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.report.read_text().splitlines()[0], "journal_redaction: fail")
        self.assertNotIn(self.values["TGM_LIVE_BOT_TOKEN"], result.stdout + result.stderr)

    def test_session_material_in_report_sets_fail(self):
        self.report.write_bytes(b"journal_redaction: not-run\n" + self.sender.read_bytes())
        self.report.chmod(0o600)
        result = self.finalize()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.report.read_text().splitlines()[0], "journal_redaction: fail")

    def test_shell_expression_is_rejected_as_config_value(self):
        marker = self.root / "executed"
        self.values["TGM_PLATFORM_API_HASH"] = f"$(touch {marker})"
        self.write_config()
        with self.assertRaises(ValueError):
            MODULE.parse_config(self.config)
        self.assertFalse(marker.exists())

    def test_missing_session_is_rejected(self):
        self.observer.unlink()
        with self.assertRaises(FileNotFoundError):
            MODULE.validate_sessions(MODULE.parse_config(self.config))

    def test_capture_filters_unit_and_refuses_secret_without_persisting_it(self):
        unit = "tgm-g4-test.service"
        records = [
            {"_SYSTEMD_USER_UNIT": "other.service", "MESSAGE": "unrelated private content"},
            {"_SYSTEMD_USER_UNIT": unit, "MESSAGE": "safe G4 output"},
            {"_SYSTEMD_USER_UNIT": unit, "MESSAGE": f"token={self.values['TGM_LIVE_BOT_TOKEN']}"},
        ]
        fake = self.root / "journalctl"
        fake.write_text(
            "#!/usr/bin/env python3\n"
            "import json\n"
            f"records = {records!r}\n"
            "for record in records:\n"
            " print(json.dumps(record))\n"
        )
        fake.chmod(0o700)
        output = self.root / "captured-journal"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--capture", "--config", str(self.config),
             "--since", "@1", "--unit", unit, "--journal", str(output)],
            capture_output=True,
            check=False,
            text=True,
            env={**os.environ, "PATH": f"{self.root}:{os.environ['PATH']}"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output.read_text(), "safe G4 output\n")
        self.assertNotIn(self.values["TGM_LIVE_BOT_TOKEN"], output.read_text())
        self.assertNotIn("unrelated private content", output.read_text())

    def test_capture_does_not_overwrite_existing_file_or_follow_symlink(self):
        unit = "tgm-g4-test.service"
        fake = self.root / "journalctl"
        fake.write_text(
            "#!/usr/bin/env python3\n"
            "import json\n"
            f"print(json.dumps({{'_SYSTEMD_USER_UNIT': {unit!r}, 'MESSAGE': 'safe'}}))\n"
        )
        fake.chmod(0o700)
        existing = self.root / "existing-journal"
        existing.write_text("do not replace\n")
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--capture", "--config", str(self.config),
             "--since", "@1", "--unit", unit, "--journal", str(existing)],
            capture_output=True,
            check=False,
            text=True,
            env={**os.environ, "PATH": f"{self.root}:{os.environ['PATH']}"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(existing.read_text(), "do not replace\n")

        target = self.root / "symlink-target"
        target.write_text("do not follow\n")
        link = self.root / "journal-link"
        link.symlink_to(target)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--capture", "--config", str(self.config),
             "--since", "@1", "--unit", unit, "--journal", str(link)],
            capture_output=True,
            check=False,
            text=True,
            env={**os.environ, "PATH": f"{self.root}:{os.environ['PATH']}"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_text(), "do not follow\n")

    def test_wrapper_uses_nonexistent_path_for_exclusive_journal_capture(self):
        fake_bin = self.root / "bin"
        fake_bin.mkdir(mode=0o700)
        temporary = self.root / "tmp"
        temporary.mkdir(mode=0o700)
        unit_file = self.root / "unit-name"
        systemctl = fake_bin / "systemctl"
        systemctl.write_text(f"#!{sys.executable}\nraise SystemExit(0)\n")
        systemctl.chmod(0o700)
        systemd_run = fake_bin / "systemd-run"
        systemd_run.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "unit = next(arg.split('=', 1)[1] for arg in sys.argv if arg.startswith('--unit='))\n"
            "open(os.environ['G4_TEST_UNIT_FILE'], 'w').write(unit)\n"
            "open(os.environ['G4_TEST_SYSTEMD_RUN_ARGS'], 'w').write('\\n'.join(sys.argv))\n"
        )
        systemd_run.chmod(0o700)
        journalctl = fake_bin / "journalctl"
        journalctl.write_text(
            f"#!{sys.executable}\n"
            "import json, os\n"
            "unit = open(os.environ['G4_TEST_UNIT_FILE']).read()\n"
            "print(json.dumps({'_SYSTEMD_USER_UNIT': unit, 'MESSAGE': 'safe test record'}))\n"
        )
        journalctl.chmod(0o700)
        environment = {
            **os.environ,
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "TMPDIR": str(temporary),
            "TGM_G4_CONFIG_FILE": str(self.config),
            "G4_TEST_UNIT_FILE": str(unit_file),
            "G4_TEST_SYSTEMD_RUN_ARGS": str(self.root / "systemd-run-args"),
        }
        result = subprocess.run(
            ["bash", str(WRAPPER)],
            cwd=Path(__file__).resolve().parents[2],
            capture_output=True,
            check=False,
            text=True,
            env=environment,
        )
        self.assertEqual(result.returncode, 3, result.stderr)
        systemd_run_args = Path(environment["G4_TEST_SYSTEMD_RUN_ARGS"]).read_text()
        self.assertIn("DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55439/tgm_test_control", systemd_run_args)
        self.assertIn("TGM_TEST_PG_PORT=55439", systemd_run_args)
        self.assertNotIn("55432", systemd_run_args)
        match = re.search(r"G4 报告：(.+)", result.stdout)
        self.assertIsNotNone(match, result.stdout)
        report_path = Path(match.group(1))
        try:
            report = report_path.read_text()
            self.assertIn("journal_redaction: pass\n", report)
            self.assertIn("flood_wait_observed: not-run\n", report)
            self.assertEqual(list(temporary.iterdir()), [report_path])
        finally:
            report_path.unlink(missing_ok=True)
        self.assertEqual(list(temporary.iterdir()), [])

    def run_journal_child(self, database_url):
        fake_bin = self.root / "child-bin"
        fake_bin.mkdir(mode=0o700)
        captured = self.root / "cargo-environment.json"
        cargo = fake_bin / "cargo"
        cargo.write_text(
            f"#!{sys.executable}\n"
            "import json, os\n"
            "from pathlib import Path\n"
            "Path(os.environ['G4_TEST_CARGO_ENV']).write_text(json.dumps({"
            "'DATABASE_URL': os.environ.get('DATABASE_URL'), "
            "'TGM_TEST_PG_PORT': os.environ.get('TGM_TEST_PG_PORT')}))\n"
        )
        cargo.chmod(0o700)
        report = self.root / "child-report"
        report.write_text("journal_redaction: not-run\n")
        report.chmod(0o600)
        environment = {
            **os.environ,
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "TGM_G4_CONFIG_FILE": str(self.config),
            "DATABASE_URL": database_url,
            "G4_TEST_CARGO_ENV": str(captured),
        }
        result = subprocess.run(
            ["bash", str(WRAPPER), "--journal-child", str(report)],
            cwd=Path(__file__).resolve().parents[2],
            capture_output=True,
            check=False,
            text=True,
            env=environment,
        )
        return result, captured

    def test_journal_child_forces_g4_database_and_port(self):
        result, captured = self.run_journal_child(
            "postgres://tgm_test:tpw@127.0.0.1:55439/tgm_test_control"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(captured.read_text()),
            {
                "DATABASE_URL": "postgres://tgm_test:tpw@127.0.0.1:55439/tgm_test_control",
                "TGM_TEST_PG_PORT": "55439",
            },
        )

    def test_journal_child_rejects_pre008_database_before_cargo(self):
        result, captured = self.run_journal_child(
            "postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control"
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("拒绝执行：G4 验收只允许本机 tgm_test_control 测试库", result.stderr)
        self.assertFalse(captured.exists())


if __name__ == "__main__":
    unittest.main()
