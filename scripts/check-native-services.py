#!/usr/bin/env python3
"""Exercise native packaging and installation without changing host services."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ROLES = ("control-plane", "bot-gateway", "mtproto-worker", "fanout", "rule", "delivery")


def executable(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8")
    path.chmod(0o755)


class NativeServices(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="tgm-native-services-test-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.home = self.base / "home"
        self.home.mkdir(mode=0o700)
        self.fake_root = self.base / "root"
        self.fake_root.mkdir(mode=0o755)
        self.commands = self.base / "commands"
        self.commands.mkdir()
        self.calls = self.base / "commands.log"
        self.environment = os.environ | {
            "HOME": str(self.home), "PATH": f"{self.commands}:{os.environ['PATH']}",
            "TMPDIR": str(self.base), "CALLS": str(self.calls),
            "PYTHONDONTWRITEBYTECODE": "1", "MOCK_UID": str(os.getuid() or 1001),
        }
        self.installer = self.base / "install-native.sh"
        source = (ROOT / "scripts/install-native.sh").read_text(encoding="utf-8")
        for original in ("/etc/systemd/system", "/etc/tgm", "/usr/local/bin", "/opt/tgm"):
            source = re.sub(r"(?<![A-Za-z0-9_./])" + re.escape(original), str(self.fake_root) + original, source)
        self.installer.write_text(source, encoding="utf-8")
        self.fixture = self.base / "fixture"
        self.fixture.mkdir()
        shutil.copytree(ROOT / "deploy/systemd", self.fixture / "deploy/systemd")
        executable(self.fixture / "bin/tgm", '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALLS"\nprintf "%s\\n" "--env-file --check-config"\n')
        executable(self.fixture / "scripts/native-release-preflight.sh", '#!/bin/sh\nprintf "preflight %s\\n" "$*" >> "$CALLS"\nexit "${PREFLIGHT_STATUS:-0}"\n')
        shutil.copyfile(self.installer, self.fixture / "scripts/install-native.sh")
        shutil.copyfile(ROOT / "scripts/verify-native-candidate.py", self.fixture / "scripts/verify-native-candidate.py")
        (self.fixture / "VERSION").write_text("0.1.0\n", encoding="utf-8")
        self.stub("cosign", 'printf "cosign %s\\n" "$*" >> "$CALLS"\nexit "${SIGNATURE_STATUS:-0}"')
        self.stub("systemctl", 'printf "systemctl %s\\n" "$*" >> "$CALLS"\n[ "$*" = "--user show-environment" ] || exit 99\nexit "${MANAGER_STATUS:-0}"')
        account_index = '''case "${2:-}" in
tgm-control-plane) account_index=0;; tgm-bot-gateway) account_index=1;; tgm-mtproto-worker) account_index=2;;
tgm-fanout) account_index=3;; tgm-rule) account_index=4;; tgm-delivery) account_index=5;; *) account_index=0;;
esac
account_number=$((2001 + account_index))
'''
        self.stub("id", account_index + 'case "$1" in -u) echo "$MOCK_UID";; -G) echo "${ACCOUNT_GID:-$account_number}${EXTRA_GROUPS:-}";; *) exit 99;; esac')
        self.stub("getent", account_index + '''case "$1:${2:-}" in
passwd:[0-9]*) printf 'tester:x:%s:2001::%s:/bin/sh\n' "$MOCK_UID" "$HOME";;
passwd:tgm-*) [ "${ACCOUNT_EXISTS:-0}" = 1 ] || [ -f "$TMPDIR/account-created-$2" ] || exit 2
 printf '%s:x:%s:%s::%s:%s\n' "$2" "${ACCOUNT_UID:-$account_number}" "${ACCOUNT_GID:-$account_number}" "/nonexistent/$2" "${ACCOUNT_SHELL:-/sbin/nologin}";;
group:tgm-*) [ "${GROUP_EXISTS:-0}" = 1 ] || [ -f "$TMPDIR/group-created-$2" ] || exit 2
 printf '%s:x:%s:%s\n' "$2" "${ACCOUNT_GID:-$account_number}" "${GROUP_MEMBERS:-}";;
shadow:tgm-*) printf '%s:%s:0:0:99999:7:::\n' "$2" "${ACCOUNT_PASSWORD:-!}";;
*) if [ "$1" = passwd ] && [ "$#" = 1 ]; then
     [ "${SHARED_PRIMARY_GROUP:-0}" = 0 ] || printf 'other:x:2002:2001::/nonexistent:/sbin/nologin\n'
   elif [ "$1" = group ] && [ "$#" = 1 ]; then
     [ "${SHARED_GROUP_GID:-0}" = 0 ] || printf 'alias:x:2001:other\n'
   else exit 2; fi;;
esac''')
        self.stub("groupadd", 'printf "groupadd %s\\n" "$*" >> "$CALLS"\ntouch "$TMPDIR/group-created-${@: -1}"')
        self.stub("useradd", 'printf "useradd %s\\n" "$*" >> "$CALLS"\ntouch "$TMPDIR/account-created-${@: -1}"')
        self.stub("nologin", "exit 1")
        self.stub("chown", 'printf "chown %s\\n" "$*" >> "$CALLS"')
        real_install = shutil.which("install")
        self.stub("install", f'''if [ "${{INSTALL_FAILURE:-0}}" = 1 ]; then case "${{*: -2:1}}" in */rule.env.example) exit 1;; esac; fi
arguments=()
while [ "$#" -gt 0 ]; do
 case "$1" in -o|-g) shift 2;; *) arguments+=("$1"); shift;; esac
done
exec {real_install} "${{arguments[@]}}"''')
        real_stat = shutil.which("stat")
        self.stub("stat", f'''if [ "$MOCK_UID" = 0 ] && [ "$1" = -c ] && [ "$2" = %u ]; then echo 0; else exec {real_stat} "$@"; fi''')
        real_ln = shutil.which("ln")
        self.stub("ln", f'''if [ "${{LINK_FAILURE:-0}}" = 1 ]; then case "${{*: -1}}" in */rule.env.example) exit 1;; esac; fi
exec {real_ln} "$@"''')
        self.archive, self.checksums, self.bundle = self.make_archive("0.1.0")

    def stub(self, name: str, body: str) -> None:
        executable(self.commands / name, "#!/usr/bin/env bash\nset -eu\n" + body + "\n")

    def make_archive(self, version: str) -> tuple[Path, Path, Path]:
        (self.fixture / "VERSION").write_text(f"{version}\n", encoding="utf-8")
        buildinfo = {"channel": "stable", "stable": True, "version": version}
        candidate = re.fullmatch(r"([0-9]+\.[0-9]+\.[0-9]+)-candidate\.([1-9][0-9]*)\.([1-9][0-9]*)\.([0-9a-f]{12})", version)
        if candidate:
            shutil.copyfile(ROOT / "scripts/verify-native-candidate.py", self.fixture / "scripts/verify-native-candidate.py")
            source_commit = candidate.group(4) + "0" * 28
            run_id = int(candidate.group(2))
            attempt = int(candidate.group(3))
            buildinfo = {
                "channel": "candidate", "stable": False, "version": version,
                "source_commit": source_commit, "candidate_run_id": run_id,
                "candidate_attempt": attempt, "ci_run_id": 456,
                "workflow_run_url": f"https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/{run_id}",
                "target": "x86_64-unknown-linux-gnu",
                "builder_image": "sha256:" + "b" * 64,
                "release_tag": f"candidate-{run_id}-a{attempt}-{candidate.group(4)}",
                "artifact": f"tgm-{version}-linux-x86_64-gnu.tar.gz",
            }
        else:
            (self.fixture / "scripts/verify-native-candidate.py").unlink(missing_ok=True)
        (self.fixture / "BUILDINFO.json").write_text(json.dumps(buildinfo), encoding="utf-8")
        (self.fixture / "sbom.cdx.json").write_text(json.dumps({"bomFormat": "CycloneDX", "version": 1}), encoding="utf-8")
        files = sorted(path for path in self.fixture.rglob("*") if path.is_file() and path.name != "SHA256SUMS")
        checksums = "".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.relative_to(self.fixture).as_posix()}\n" for path in files)
        (self.fixture / "SHA256SUMS").write_text(checksums, encoding="utf-8")
        archive = self.base / f"tgm-{version}-linux-x86_64-gnu.tar.gz"
        with tarfile.open(archive, "w:gz") as stream:
            for path in sorted(self.fixture.rglob("*")):
                stream.add(path, arcname="./" + path.relative_to(self.fixture).as_posix(), recursive=False)
        outer = self.base / f"checksums-{version}"
        outer_files = [(archive, archive.name)]
        if candidate:
            outer_files.extend((
                (self.fixture / "BUILDINFO.json", "candidate-BUILDINFO.json"),
                (self.fixture / "sbom.cdx.json", "candidate-sbom.cdx.json"),
            ))
        outer.write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n"
            for path, name in outer_files
        ), encoding="utf-8")
        release_buildinfo = self.base / "candidate-BUILDINFO.json"
        release_sbom = self.base / "candidate-sbom.cdx.json"
        shutil.copyfile(self.fixture / "BUILDINFO.json", release_buildinfo)
        shutil.copyfile(self.fixture / "sbom.cdx.json", release_sbom)
        bundle = self.base / "bundle.json"
        bundle.write_text("{}\n", encoding="utf-8")
        if candidate:
            archive_digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            provenance = {
                "_type": "https://in-toto.io/Statement/v1",
                "subject": [{"name": archive.name, "digest": {"sha256": archive_digest}}],
                "predicateType": "https://slsa.dev/provenance/v1",
                "predicate": {
                    "buildDefinition": {
                        "buildType": "https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-candidate.yml",
                        "externalParameters": {
                            "channel": "candidate", "candidate_tag": buildinfo["release_tag"],
                            "candidate_version": version, "candidate_run_id": str(run_id),
                            "candidate_attempt": str(attempt), "ci_run_id": "456",
                            "target": buildinfo["target"], "builder_image": buildinfo["builder_image"],
                        },
                        "resolvedDependencies": [{
                            "uri": "git+https://github.com/wtfdelphia/tg-monitor-bot@refs/heads/dev",
                            "digest": {"gitCommit": buildinfo["source_commit"]},
                        }],
                    },
                    "runDetails": {
                        "builder": {"id": f"https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/{run_id}"},
                        "metadata": {
                            "invocationId": f"https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/{run_id}/attempts/{attempt}"
                        },
                    },
                },
            }
            self.provenance = self.base / "provenance.json"
            self.provenance.write_text(json.dumps(provenance), encoding="utf-8")
            self.provenance_bundle = self.base / "provenance.sigstore.json"
            self.provenance_bundle.write_text("{}\n", encoding="utf-8")
            self.buildinfo_path = self.fixture / "BUILDINFO.json"
        else:
            self.provenance = None
            self.provenance_bundle = None
        return archive, outer, bundle

    def invoke(self, *arguments: str, **environment: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["bash", str(self.installer), *arguments], env=self.environment | environment,
                              text=True, capture_output=True, timeout=20)

    def install(self, mode: str = "user", accept: bool = True, **environment: str) -> subprocess.CompletedProcess[str]:
        arguments = [f"--{mode}", "--app-services"]
        if mode == "user" and accept:
            arguments.append("--accept-same-uid")
        return self.invoke(*arguments, "--archive", str(self.archive), "--checksums", str(self.checksums),
                           "--bundle", str(self.bundle), **environment)

    def install_candidate(self, **environment: str) -> subprocess.CompletedProcess[str]:
        return self.invoke(
            "--user", "--candidate", "--accept-candidate", "--archive", str(self.archive),
            "--checksums", str(self.checksums), "--bundle", str(self.bundle),
            "--provenance", str(self.provenance), "--provenance-bundle", str(self.provenance_bundle),
            "--buildinfo", str(self.base / "candidate-BUILDINFO.json"),
            "--sbom", str(self.base / "candidate-sbom.cdx.json"),
            **environment,
        )

    def assert_success(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_rejected(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.home / ".local/bin/tgm").exists())
        self.assertFalse((self.fake_root / "usr/local/bin/tgm").exists())

    def test_user_install_and_rollback_do_not_change_service_state(self) -> None:
        self.assert_success(self.install())
        unit = self.home / ".config/systemd/user/tgm-app@.service"
        self.assertEqual(unit.read_bytes(), (ROOT / "deploy/systemd/app/user/tgm-app@.service").read_bytes())
        for role in ROLES:
            example = self.home / f".config/tgm/app/{role}.env.example"
            self.assertEqual(example.stat().st_mode & 0o777, 0o600)
            self.assertFalse(example.with_suffix("").exists())
        self.assertEqual(list(self.fake_root.rglob("*.service")), [])
        self.archive, self.checksums, self.bundle = self.make_archive("0.1.1")
        self.assert_success(self.install())
        self.assert_success(self.invoke("--user", "--rollback", "0.1.0"))
        self.assertIn("/0.1.0/tgm", os.readlink(self.home / ".local/bin/tgm"))
        calls = self.calls.read_text(encoding="utf-8")
        self.assertNotIn("migrate", calls)
        self.assertNotIn("cleanup", calls)
        for line in calls.splitlines():
            if line.startswith("systemctl "):
                self.assertEqual(line, "systemctl --user show-environment")

    def test_user_requires_same_uid_acceptance_and_manager(self) -> None:
        self.assert_rejected(self.install(accept=False))
        self.assert_rejected(self.install(MANAGER_STATUS="1"))
        self.assert_rejected(self.install(MOCK_UID="0"))

    def test_conflicts_symlinks_and_writable_parent_are_rejected(self) -> None:
        unit = self.home / ".config/systemd/user/tgm-app@.service"
        unit.parent.mkdir(parents=True)
        unit.write_text("existing unit", encoding="utf-8")
        self.assert_rejected(self.install())
        self.assertEqual(unit.read_text(), "existing unit")
        unit.unlink()
        unit.symlink_to(ROOT / "deploy/systemd/app/user/tgm-app@.service")
        self.assert_rejected(self.install())
        unit.unlink()
        self.home.chmod(0o777)
        self.assert_rejected(self.install())
        self.home.chmod(0o700)
        shutil.rmtree(self.home / ".config")
        (self.home / ".config").symlink_to(self.fake_root)
        self.assert_rejected(self.install())

    def test_failed_checks_and_copy_leave_current_version_unchanged(self) -> None:
        self.assert_rejected(self.install(SIGNATURE_STATUS="1"))
        self.assert_rejected(self.install(PREFLIGHT_STATUS="1"))
        self.assert_rejected(self.install(INSTALL_FAILURE="1"))

    def test_candidate_install_requires_explicit_nonproduction_path(self) -> None:
        version = "0.1.0-candidate.12345.1.0123456789ab"
        self.archive, self.checksums, self.bundle = self.make_archive(version)
        ordinary = self.invoke(
            "--user", "--archive", str(self.archive), "--checksums", str(self.checksums),
            "--bundle", str(self.bundle),
        )
        self.assert_rejected(ordinary)
        missing_confirmation = self.invoke(
            "--user", "--candidate", "--archive", str(self.archive), "--checksums", str(self.checksums),
            "--bundle", str(self.bundle), "--provenance", str(self.provenance),
            "--provenance-bundle", str(self.provenance_bundle),
        )
        self.assert_rejected(missing_confirmation)
        self.assert_rejected(self.install_candidate(MOCK_UID="0"))
        self.assert_rejected(self.invoke(
            "--user", "--rollback", "0.1.0", "--buildinfo", str(self.fixture / "BUILDINFO.json"),
        ))
        self.assert_rejected(self.invoke(
            "--root", "--candidate", "--accept-candidate", "--archive", str(self.archive),
            "--checksums", str(self.checksums), "--bundle", str(self.bundle),
            "--provenance", str(self.provenance), "--provenance-bundle", str(self.provenance_bundle),
            MOCK_UID="0",
        ))
        self.assert_rejected(self.invoke(
            "--user", "--candidate", "--accept-candidate", "--app-services",
            "--archive", str(self.archive), "--checksums", str(self.checksums),
            "--bundle", str(self.bundle), "--provenance", str(self.provenance),
            "--provenance-bundle", str(self.provenance_bundle),
        ))
        stable_link = self.home / ".local/bin/tgm"
        stable_link.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        (self.home / ".local").chmod(0o700)
        stable_link.parent.chmod(0o700)
        stable_link.symlink_to("/existing/stable/tgm")
        self.assert_success(self.install_candidate())
        candidate_binary = self.home / ".local/opt/tgm/candidates/0.1.0-candidate.12345.1.0123456789ab/bin/tgm"
        self.assertTrue(candidate_binary.is_file())
        self.assertEqual(os.readlink(stable_link), "/existing/stable/tgm")
        calls = self.calls.read_text(encoding="utf-8")
        self.assertIn("--certificate-identity https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-candidate.yml@refs/heads/dev", calls)
        self.assertIn("provenance.sigstore.json", calls)
        self.assertFalse(any(line.startswith("systemctl ") for line in calls.splitlines()))

    def test_candidate_install_rejects_mismatched_release_buildinfo(self) -> None:
        version = "0.1.0-candidate.12348.1.abcdef012345"
        self.archive, self.checksums, self.bundle = self.make_archive(version)
        (self.base / "candidate-BUILDINFO.json").write_text('{"channel":"candidate"}\n', encoding="utf-8")
        self.assert_rejected(self.install_candidate())

    def test_candidate_install_rejects_mismatched_workflow_run_url(self) -> None:
        version = "0.1.0-candidate.12349.1.abcdef012345"
        self.archive, self.checksums, self.bundle = self.make_archive(version)
        buildinfo_path = self.base / "candidate-BUILDINFO.json"
        buildinfo = json.loads(buildinfo_path.read_text(encoding="utf-8"))
        buildinfo["workflow_run_url"] = "https://github.com/example/repo/actions/runs/12349"
        buildinfo_path.write_text(json.dumps(buildinfo), encoding="utf-8")
        self.assert_rejected(self.install_candidate())

    def test_candidate_install_rejects_tampered_provenance(self) -> None:
        version = "0.1.0-candidate.12346.1.abcdef012345"
        self.archive, self.checksums, self.bundle = self.make_archive(version)
        original = json.loads(self.provenance.read_text(encoding="utf-8"))
        mutations = (
            lambda value: value["predicate"]["buildDefinition"]["resolvedDependencies"][0]["digest"].update(gitCommit="f" * 40),
            lambda value: value["predicate"]["buildDefinition"]["externalParameters"].update(candidate_attempt="2"),
            lambda value: value["predicate"]["buildDefinition"]["externalParameters"].update(builder_image="sha256:other"),
            lambda value: value["predicate"]["buildDefinition"]["resolvedDependencies"][0].update(uri="git+https://example.invalid/repo@refs/heads/dev"),
            lambda value: value["predicate"]["runDetails"]["metadata"].update(invocationId="other"),
            lambda value: value["subject"][0]["digest"].update(sha256="0" * 64),
        )
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                provenance = json.loads(json.dumps(original))
                mutate(provenance)
                self.provenance.write_text(json.dumps(provenance), encoding="utf-8")
                self.assert_rejected(self.install_candidate())

    def test_candidate_install_rejects_extra_outer_checksum_entries(self) -> None:
        version = "0.1.0-candidate.12347.1.abcdef012345"
        self.archive, self.checksums, self.bundle = self.make_archive(version)
        self.checksums.write_text(
            self.checksums.read_text(encoding="ascii") + "0" * 64 + "  unexpected.tar.gz\n",
            encoding="ascii",
        )
        self.assert_rejected(self.install_candidate())

    def test_failed_update_keeps_existing_binary_unit_and_config(self) -> None:
        self.assert_success(self.install())
        link = self.home / ".local/bin/tgm"
        previous = os.readlink(link)
        unit = self.home / ".config/systemd/user/tgm-app@.service"
        original = unit.read_bytes()
        actual = self.home / ".config/tgm/app/rule.env"
        actual.write_text("existing-private-config\n", encoding="utf-8")
        actual.chmod(0o600)
        self.archive, self.checksums, self.bundle = self.make_archive("0.1.1")
        for environment in ({"SIGNATURE_STATUS": "1"}, {"PREFLIGHT_STATUS": "1"}):
            self.assertNotEqual(self.install(**environment).returncode, 0)
            self.assertEqual(os.readlink(link), previous)
        example = self.home / ".config/tgm/app/rule.env.example"
        example.unlink()
        result = self.install(LINK_FAILURE="1")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("工件无法落地", result.stderr)
        self.assertEqual(os.readlink(link), previous)
        self.assertEqual(unit.read_bytes(), original)
        self.assertEqual(actual.read_text(), "existing-private-config\n")
        self.assertFalse(example.exists())
        self.assertIn("已创建版本目录", result.stdout)

    def test_root_account_boundaries_and_only_application_units(self) -> None:
        self.assert_success(self.install("root", MOCK_UID="0"))
        self.assertTrue((self.fake_root / "etc/systemd/system/tgm-app@.service").is_file())
        self.assertEqual(len(list((self.fake_root / "etc/systemd/system").iterdir())), 1)
        self.assertEqual(len(list((self.fake_root / "etc/tgm/app").iterdir())), len(ROLES))
        calls = self.calls.read_text()
        self.assertNotIn("systemctl ", calls)
        for role in ROLES:
            self.assertIn(f"groupadd --system tgm-{role}", calls)
            self.assertRegex(calls, rf"useradd --system --gid tgm-{role} .* tgm-{role}\n")

    def test_unsafe_existing_service_accounts_are_rejected(self) -> None:
        for invalid in (
            {"ACCOUNT_UID": "0"}, {"ACCOUNT_GID": "0"}, {"ACCOUNT_SHELL": "/bin/bash"},
            {"EXTRA_GROUPS": " 27"}, {"GROUP_MEMBERS": "other"},
            {"ACCOUNT_PASSWORD": "unlocked"}, {"SHARED_PRIMARY_GROUP": "1"}, {"SHARED_GROUP_GID": "1"},
        ):
            with self.subTest(invalid=invalid):
                self.assert_rejected(self.install("root", MOCK_UID="0", ACCOUNT_EXISTS="1", GROUP_EXISTS="1", **invalid))

    def test_old_app_incompatible_version_cannot_be_rolled_back(self) -> None:
        self.assert_success(self.install())
        old = self.home / ".local/opt/tgm/releases/0.0.1"
        executable(old / "bin/tgm", "#!/bin/sh\nexit 0\n")
        (old / "tgm").symlink_to("bin/tgm")
        executable(old / "scripts/native-release-preflight.sh", "#!/bin/sh\nexit 1\n")
        previous = os.readlink(self.home / ".local/bin/tgm")
        result = self.invoke("--user", "--rollback", "0.0.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(os.readlink(self.home / ".local/bin/tgm"), previous)
        self.assertNotEqual(self.invoke("--user", "--app-services", "--rollback", "0.1.0").returncode, 0)

    def test_binary_only_update_cannot_bypass_installed_app_compatibility(self) -> None:
        self.assert_success(self.install())
        previous = os.readlink(self.home / ".local/bin/tgm")
        (self.fixture / "deploy/systemd/app/user/tgm-app@.service").write_text("incompatible unit\n", encoding="utf-8")
        self.archive, self.checksums, self.bundle = self.make_archive("0.1.1")
        result = self.invoke("--user", "--archive", str(self.archive), "--checksums", str(self.checksums), "--bundle", str(self.bundle))
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(os.readlink(self.home / ".local/bin/tgm"), previous)
        self.assertFalse((self.home / ".local/opt/tgm/releases/0.1.1").exists())

    def test_package_contains_application_tree_and_is_deterministic(self) -> None:
        binary = self.fixture / "bin/tgm"
        sbom = self.base / "sbom.json"
        sbom.write_text(json.dumps({"bomFormat": "CycloneDX"}), encoding="utf-8")
        archives = []
        for suffix in ("first", "second"):
            output = self.base / suffix
            result = subprocess.run([
                "python3", str(ROOT / "scripts/native-release-package.py"), "--binary", str(binary),
                "--sbom", str(sbom), "--version", "0.1.0", "--source-commit", "0" * 40,
                "--rust-toolchain", "test", "--builder-image", "test", "--source-date-epoch", "1700000000",
                "--output", str(output),
            ], env=self.environment, text=True, capture_output=True, timeout=20)
            self.assert_success(result)
            archives.append(output / "tgm-0.1.0-linux-x86_64-gnu.tar.gz")
        self.assertEqual(archives[0].read_bytes(), archives[1].read_bytes())
        with tarfile.open(archives[0]) as stream:
            names = stream.getnames()
            self.assertIn("./deploy/systemd/app/user/tgm-app@.service", names)
            self.assertIn("./deploy/systemd/app/tgm-app@.service", names)
            for role in ROLES:
                self.assertIn(f"./deploy/systemd/app/{role}.env.example", names)

    def test_candidate_package_is_marked_non_stable_and_unique(self) -> None:
        binary = self.fixture / "bin/tgm"
        sbom = self.base / "candidate-sbom.json"
        sbom.write_text(json.dumps({"bomFormat": "CycloneDX"}), encoding="utf-8")
        commit = "a" * 40
        result = subprocess.run([
            "python3", str(ROOT / "scripts/native-release-package.py"), "--binary", str(binary),
            "--sbom", str(sbom), "--version", "0.1.0", "--channel", "candidate",
            "--candidate-run-id", "123", "--candidate-attempt", "2", "--candidate-ci-run-id", "456",
            "--candidate-run-url", "https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/123",
            "--source-commit", commit, "--rust-toolchain", "test", "--builder-image", "test",
            "--source-date-epoch", "1700000000", "--output", str(self.base / "candidate-package"),
        ], env=self.environment, text=True, capture_output=True, timeout=20)
        self.assert_success(result)
        archive = self.base / "candidate-package/tgm-0.1.0-candidate.123.2.aaaaaaaaaaaa-linux-x86_64-gnu.tar.gz"
        self.assertTrue(archive.is_file())
        with tarfile.open(archive) as stream:
            package_root = Path(stream.getnames()[0].removeprefix("./"))
            buildinfo_member = next(name for name in stream.getnames() if name.endswith("/BUILDINFO.json"))
            readme_member = next(name for name in stream.getnames() if name.endswith("/README.md"))
            buildinfo = json.load(stream.extractfile(buildinfo_member))
            readme = stream.extractfile(readme_member).read().decode()
        self.assertEqual(buildinfo["channel"], "candidate")
        self.assertIs(buildinfo["stable"], False)
        self.assertEqual(buildinfo["source_commit"], commit)
        self.assertEqual(buildinfo["ci_run_id"], 456)
        self.assertEqual(buildinfo["workflow_run_url"], "https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/123")
        self.assertNotIn("\\n+", readme)
        self.assertIn("CANDIDATE / NOT STABLE", readme)
        self.assertIn("candidate-provenance.json", readme)
        self.assertIn("--accept-candidate", readme)
        self.assertIn("预先安装 Python 3", readme)
        self.assertNotIn("\n+", readme)
        self.assertNotIn("@CANDIDATE_", readme)

    def test_stable_package_readme_does_not_advertise_candidate_assets(self) -> None:
        binary = self.fixture / "bin/tgm"
        sbom = self.base / "stable-sbom.json"
        sbom.write_text(json.dumps({"bomFormat": "CycloneDX"}), encoding="utf-8")
        output = self.base / "stable-package"
        result = subprocess.run([
            "python3", str(ROOT / "scripts/native-release-package.py"), "--binary", str(binary),
            "--sbom", str(sbom), "--version", "0.1.0", "--source-commit", "0" * 40,
            "--rust-toolchain", "test", "--builder-image", "test", "--source-date-epoch", "1700000000",
            "--output", str(output),
        ], env=self.environment, text=True, capture_output=True, timeout=20)
        self.assert_success(result)
        with tarfile.open(output / "tgm-0.1.0-linux-x86_64-gnu.tar.gz") as stream:
            readme_member = next(name for name in stream.getnames() if name.endswith("/README.md"))
            readme = stream.extractfile(readme_member).read().decode()
            buildinfo_member = next(name for name in stream.getnames() if name.endswith("/BUILDINFO.json"))
            buildinfo = json.load(stream.extractfile(buildinfo_member))
        self.assertNotIn("candidate-SHA256SUMS", readme)
        self.assertNotIn("@CANDIDATE_", readme)
        self.assertNotIn("发布通道：", readme)
        self.assertNotIn("channel", buildinfo)
        self.assertNotIn("stable", buildinfo)

    def test_preflight_rejects_unsupported_directives_even_when_verify_returns_zero(self) -> None:
        self.stub("ldd", 'if [ "$1" = --version ]; then echo "ldd (GNU libc) 2.17"; else echo "libc.so.6 => /lib64/libc.so.6"; fi')
        self.stub("openssl", "echo 'OpenSSL 1.0.2k'")
        self.stub("systemd-analyze", 'if [ "$1" = --version ]; then echo "systemd 219"; elif [ "$1" = verify ]; then printf "%s\\n" "${VERIFY_WARNING:-}" >&2; else exit 99; fi')
        for warning, expected in (("", 0), ("tgm-app@.service: Unknown lvalue 'Broken' in section 'Service'", 1), ("tgm-app@.service: Failed to parse", 1), ("snapd.service: Unknown key name 'RestartMode'", 0)):
            result = subprocess.run([
                "bash", str(ROOT / "scripts/native-release-preflight.sh"), "--app-services",
                "--binary", str(self.fixture / "bin/tgm"), "--unit-dir", str(ROOT / "deploy/systemd"),
            ], env=self.environment | {"VERIFY_WARNING": warning}, text=True, capture_output=True, timeout=20)
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
