import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parents[1]


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


INPUTS = load("g1_inputs", "g1-external-input-check.py")
PRESTAGE = load("prestage", "prestage-local.py")


class InputTests(unittest.TestCase):
    def setUp(self):
        self.values = {
            "TGM_OIDC_BOT_ID": "123456", "TGM_OIDC_CLIENT_SECRET": "test-value-never-print",
            "TGM_OIDC_ALG": "RS256", "TGM_OIDC_REDIRECT_URL": "https://login.example/auth/oidc/callback",
            "TGM_OIDC_ALLOWED_ORIGIN": "https://login.example",
            "TGM_OIDC_ALLOWED_CALLBACK": "https://login.example/auth/oidc/callback",
            "TGM_OIDC_BOTFATHER_CONFIRMED": "yes",
        }
        self.tenant = {
            "operator_reference": "operator-a", "tenant_reference": "tenant-a", "applied_at": "2026-11-16",
            "evidence_reference": "private-record", "reviewer": "reviewer-a", "result": "pass",
            "applicant_is_tenant": True, "platform_credentials_reused": False,
        }

    def test_complete_inputs_do_not_execute_a_login(self):
        self.assertEqual(INPUTS.check(self.values, self.tenant), [])

    def test_tenant_evidence_requires_a_private_existing_nonempty_file(self):
        self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "tenant-application.txt"
            self.tenant["evidence_reference"] = str(evidence)
            self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))
            evidence.touch(mode=0o600)
            self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))
            evidence.write_text("redacted tenant application record\n")
            self.assertTrue(INPUTS.tenant_evidence_available(self.tenant))
            evidence.chmod(0o644)
            self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))
            evidence.chmod(0o600)
            link = Path(directory) / "linked-record"
            link.symlink_to(evidence)
            self.tenant["evidence_reference"] = str(link)
            self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))
            self.tenant["evidence_reference"] = directory
            self.assertFalse(INPUTS.tenant_evidence_available(self.tenant))

    def test_missing_tenant_evidence_cannot_produce_ready_output(self):
        with tempfile.TemporaryDirectory() as directory:
            environment = Path(directory) / "oidc.env"
            environment.write_text("".join(key + "=" + value + "\n" for key, value in self.values.items()))
            environment.chmod(0o600)
            record = Path(directory) / "tenant.json"
            record.write_text(json.dumps(self.tenant))
            record.chmod(0o600)
            output = io.StringIO()
            with patch("sys.argv", ["check", "--env-file", str(environment), "--tenant-record", str(record)]), contextlib.redirect_stdout(output):
                result = INPUTS.main()
            self.assertEqual(result, 2)
            self.assertEqual(json.loads(output.getvalue())["input_readiness"], "blocked-input")
            self.assertNotIn("test-value-never-print", output.getvalue())

    def test_missing_credentials_are_only_reported_by_name(self):
        self.values["TGM_OIDC_CLIENT_SECRET"] = ""
        issues = INPUTS.check(self.values, self.tenant)
        self.assertIn("缺少 TGM_OIDC_CLIENT_SECRET", issues)
        self.assertNotIn("test-value-never-print", str(issues))

    def test_profile_incompatible_algorithms_are_rejected(self):
        for algorithm in ("EdDSA", "ES256K", "none"):
            self.values["TGM_OIDC_ALG"] = algorithm
            self.assertTrue(INPUTS.check(self.values, self.tenant))

    def test_callback_binding_and_scheme_are_checked(self):
        for callback in ("http://login.example/auth/oidc/callback", "https://login.example/wrong",
                         "https://login.example/auth/oidc/callback?code=secret", "https://user:pw@login.example/auth/oidc/callback"):
            self.values["TGM_OIDC_REDIRECT_URL"] = callback
            self.assertTrue(INPUTS.check(self.values, self.tenant))

    def test_platform_application_is_not_tenant_evidence(self):
        self.tenant["platform_credentials_reused"] = True
        self.assertTrue(INPUTS.check(self.values, self.tenant))
        self.tenant["platform_credentials_reused"] = False
        self.tenant["applicant_is_tenant"] = False
        self.assertTrue(INPUTS.check(self.values, self.tenant))

    def test_malformed_tenant_record_does_not_become_ready(self):
        with self.assertRaises(ValueError):
            INPUTS.check(self.values, [])
        self.tenant["operator_reference"] = 123
        self.assertTrue(INPUTS.check(self.values, self.tenant))

    def test_private_input_mode_and_symlink_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.env"
            path.write_text("NAME=value\n")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                INPUTS.read_env(path)
            path.chmod(0o600)
            self.assertEqual(INPUTS.read_env(path), {"NAME": "value"})
            link = Path(directory) / "link"
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                INPUTS.read_env(link)

    def test_env_shell_syntax_is_never_executed(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "executed"
            path = Path(directory) / "input.env"
            path.write_text(f'NAME="$(touch {marker})"\n')
            path.chmod(0o600)
            self.assertEqual(INPUTS.read_env(path)["NAME"], f"$(touch {marker})")
            self.assertFalse(marker.exists())
            path.write_text("NAME=first\nNAME=second\n")
            with self.assertRaises(ValueError):
                INPUTS.read_env(path)


class EvidenceReviewTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.evidence = Path(self.directory.name) / "evidence.txt"
        self.content = "申请日期：2026-9-25\r\napi_hash：未记录\r\n".encode()
        self.evidence.write_bytes(self.content)
        self.evidence.chmod(0o600)
        self.tenant = {"applied_at": "2026-09-25", "evidence_reference": str(self.evidence)}
        self.signoff = "目标租户证据 SHA-256：" + hashlib.sha256(self.content).hexdigest() + "\n"

    def test_matching_dates_and_exact_file_bytes_do_not_assert_signoff(self):
        result = INPUTS.review_tenant_evidence(self.tenant, self.signoff)
        self.assertEqual(result["status"], "consistent-pending-review")
        self.assertEqual(result["issues"], [])
        self.assertEqual(result["formal_signoff"], "not-assessed")

    def test_conflicting_date_and_hash_require_confirmation(self):
        self.tenant["applied_at"] = "2026-11-17"
        result = INPUTS.review_tenant_evidence(self.tenant, "目标租户证据 SHA-256：" + "0" * 64)
        self.assertEqual(result["status"], "blocked-confirmation")
        self.assertEqual(len(result["issues"]), 2)

    def test_invalid_and_duplicate_dates_are_rejected_without_echoing_content(self):
        for text in ("申请日期：2026-02-30", "申请日期：secret-value", "申请日期：2026-9-25\n申请日期：2026-9-26"):
            self.evidence.write_text(text)
            with self.assertRaises(ValueError) as error:
                INPUTS.review_tenant_evidence(self.tenant, self.signoff)
            self.assertNotIn("secret-value", str(error.exception))

    def test_invalid_and_duplicate_signoff_hashes_are_rejected(self):
        for text in ("目标租户证据 SHA-256：secret-value", self.signoff + self.signoff, ""):
            with self.assertRaises(ValueError) as error:
                INPUTS.review_tenant_evidence(self.tenant, text)
            self.assertNotIn("secret-value", str(error.exception))

    def test_public_or_linked_evidence_is_rejected(self):
        self.evidence.chmod(0o644)
        with self.assertRaises(ValueError):
            INPUTS.review_tenant_evidence(self.tenant, self.signoff)
        self.evidence.chmod(0o600)
        linked = self.evidence.with_name("linked.txt")
        linked.symlink_to(self.evidence)
        self.tenant["evidence_reference"] = str(linked)
        with self.assertRaises(ValueError):
            INPUTS.review_tenant_evidence(self.tenant, self.signoff)

    def test_cli_keeps_readiness_separate_and_returns_two_on_conflict(self):
        directory = Path(self.directory.name)
        environment = directory / "oidc.env"
        values = {
            "TGM_OIDC_BOT_ID": "123456", "TGM_OIDC_CLIENT_SECRET": "test-value-never-print",
            "TGM_OIDC_ALG": "RS256", "TGM_OIDC_REDIRECT_URL": "https://login.example/auth/oidc/callback",
            "TGM_OIDC_ALLOWED_ORIGIN": "https://login.example",
            "TGM_OIDC_ALLOWED_CALLBACK": "https://login.example/auth/oidc/callback",
            "TGM_OIDC_BOTFATHER_CONFIRMED": "yes",
        }
        environment.write_text("".join(key + "=" + value + "\n" for key, value in values.items()))
        record = directory / "tenant.json"
        tenant = dict(self.tenant, operator_reference="operator-a", tenant_reference="tenant-a",
                      applied_at="2026-11-17", reviewer="reviewer-a", result="pass",
                      applicant_is_tenant=True, platform_credentials_reused=False)
        record.write_text(json.dumps(tenant))
        signoff = directory / "signoff.md"
        signoff.write_text(self.signoff)
        for path in (environment, record, signoff):
            path.chmod(0o600)
        output = io.StringIO()
        arguments = ["check", "--env-file", str(environment), "--tenant-record", str(record), "--signoff-file", str(signoff)]
        with patch("sys.argv", arguments), patch.object(INPUTS, "provider_summary", side_effect=AssertionError("unexpected network")), contextlib.redirect_stdout(output):
            result = INPUTS.main()
        self.assertEqual(result, 2)
        summary = json.loads(output.getvalue())
        self.assertEqual(summary["input_readiness"], "ready")
        self.assertEqual(summary["tenant_evidence_file"], "available-review-blocked")
        self.assertEqual(summary["tenant_evidence_review"]["status"], "blocked-confirmation")
        self.assertNotIn("test-value-never-print", output.getvalue())
        self.assertNotIn("operator-a", output.getvalue())

        tenant["applied_at"] = "2026-09-25"
        record.write_text(json.dumps(tenant))
        output = io.StringIO()
        with patch("sys.argv", arguments), patch.object(INPUTS, "provider_summary", side_effect=AssertionError("unexpected network")), contextlib.redirect_stdout(output):
            result = INPUTS.main()
        self.assertEqual(result, 0)
        summary = json.loads(output.getvalue())
        self.assertEqual(summary["input_readiness"], "ready")
        self.assertEqual(summary["tenant_evidence_file"], "available-consistency-checked")
        self.assertEqual(summary["tenant_evidence_review"]["formal_signoff"], "not-assessed")

        for failure in (OSError("private-fault-value"), ValueError("private-fault-value"), KeyError("private-fault-value")):
            with self.subTest(failure=type(failure).__name__):
                output = io.StringIO()
                with patch("sys.argv", arguments + ["--provider"]), patch.object(INPUTS, "provider_summary", side_effect=failure), contextlib.redirect_stdout(output):
                    result = INPUTS.main()
                self.assertEqual(result, 1)
                summary = json.loads(output.getvalue())
                self.assertEqual(summary["input_readiness"], "ready")
                self.assertEqual(summary["tenant_evidence_review"]["issues"], [])
                self.assertEqual(summary["provider"]["status"], "check-error")
                self.assertEqual(summary["provider"]["provider_rotation"], "not-run")
                self.assertNotIn("private-fault-value", output.getvalue())
                self.assertNotIn("test-value-never-print", output.getvalue())


class BackupTests(unittest.TestCase):
    def test_schedule_uses_system_sg_and_preserves_the_new_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            stage = PRESTAGE.Stage(directory / "state")
            stage.state.mkdir(mode=0o700)
            stage.config = {"project": "tgm-prestage-0123456789", "backup": {"sha256": "old"}}
            socket = directory / "docker.sock"
            socket.touch()
            commands = []

            def run(arguments):
                commands.append(arguments)
                return subprocess.CompletedProcess(arguments, 0, b"", b"")

            def backup():
                stage.config["backup"] = {"sha256": "new"}
                stage.save()

            with patch.object(PRESTAGE.Path, "home", return_value=directory), patch.object(
                    stage, "run", side_effect=run), patch.object(stage, "backup", side_effect=backup), patch.object(
                    PRESTAGE.shutil, "which", return_value="/usr/bin/sg") as which, patch.object(
                    PRESTAGE, "DOCKER_SOCKET", socket):
                stage.schedule(True)
            which.assert_called_once_with("sg", path=os.defpath)
            unit = (directory / ".config/systemd/user/tgm-prestage-0123456789-backup.service").read_text()
            self.assertIn('ExecStart="/usr/bin/sg"', unit)
            self.assertIn('"-c"', unit)
            self.assertIn(str(stage.state / "backup-runner.py"), unit)
            self.assertNotIn(["systemctl", "--user", "start", "tgm-prestage-0123456789-backup.service"], commands)
            self.assertEqual(json.loads(stage.config_path.read_text())["backup"]["sha256"], "new")

    def test_unavailable_database_is_counted_as_backup_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = PRESTAGE.Stage(Path(directory))
            stage.config = {"project": "tgm-prestage-0123456789", "backup_failures": 0,
                            "backup": {"sha256": "previous-success"}}
            with patch.object(stage, "container", side_effect=RuntimeError("unavailable")):
                for attempt in range(2):
                    with self.assertRaises(RuntimeError):
                        stage.backup()
            self.assertEqual(stage.config["backup_failures"], 2)
            self.assertEqual(stage.config["backup"]["sha256"], "previous-success")
            events = (Path(directory) / "events.jsonl").read_text()
            self.assertIn('"event": "alert.backup_failed_twice"', events)

    def test_failed_backups_keep_previous_success_and_raise_local_alert(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = PRESTAGE.Stage(Path(directory))
            previous = {"sha256": "previous-success"}
            stage.config = {"project": "tgm-prestage-0123456789", "backup": previous, "backup_failures": 0}
            with patch.object(stage, "container", return_value="local-test"), patch.object(
                PRESTAGE.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"", b"fault")
            ):
                stage.backup(fault=True)
                stage.backup(fault=True)
            self.assertEqual(stage.config["backup"], previous)
            self.assertFalse((Path(directory) / "backups/pending.dump").exists())
            events = [json.loads(line) for line in (Path(directory) / "events.jsonl").read_text().splitlines()]
            self.assertEqual([event["event"] for event in events], [
                "backup.failed", "backup.failed", "alert.backup_failed_twice"])
            self.assertEqual(events[-1]["delivery"], "external-not-run")

    def test_remote_docker_and_inherited_secrets_are_not_forwarded(self):
        with patch.dict(os.environ, {"TGM_LIVE_BOT_TOKEN": "do-not-forward", "DATABASE_URL": "do-not-forward",
                                     "DOCKER_HOST": "tcp://remote.example:2375"}):
            environment = PRESTAGE.clean_environment()
        self.assertNotIn("TGM_LIVE_BOT_TOKEN", environment)
        self.assertNotIn("DATABASE_URL", environment)
        self.assertEqual(environment["DOCKER_HOST"], "unix:///var/run/docker.sock")

    def test_state_rejects_other_projects(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "state.json").write_text(json.dumps({"project": "production-stack"}))
            (path / "state.json").chmod(0o600)
            with self.assertRaises(RuntimeError):
                PRESTAGE.Stage(path)

    def test_operations_are_exclusive_and_reload_the_locked_state(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            first = PRESTAGE.Stage(path)
            second = PRESTAGE.Stage(path)
            with first.lock():
                first.config = {"project": "tgm-prestage-0123456789", "backup_failures": 2}
                first.save()
                with self.assertRaises(RuntimeError):
                    with second.lock():
                        self.fail("overlapping operation entered")
            with second.lock():
                self.assertEqual(second.config["backup_failures"], 2)

    def test_state_rejects_readable_or_symlinked_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            config = path / "state.json"
            config.write_text('{"project":"tgm-prestage-0123456789"}')
            config.chmod(0o644)
            with self.assertRaises(RuntimeError):
                PRESTAGE.Stage(path)
            config.unlink()
            config.symlink_to(path / "missing")
            with self.assertRaises(RuntimeError):
                PRESTAGE.Stage(path)


if __name__ == "__main__":
    unittest.main()
