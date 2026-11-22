from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("stable_release_approval", ROOT / "scripts/stable-release-approval.py")
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class StableReleaseApprovalTests(unittest.TestCase):
    def setUp(self) -> None:
        from scripts.tests.test_stable_release_assets import StableReleaseAssetsTests

        self.fixture = StableReleaseAssetsTests(methodName="test_prepare_and_verify_bind_exact_payloads")
        self.fixture.setUp()
        self.assets = self.fixture.assets
        self.version = self.fixture.version
        self.commit = self.fixture.commit
        self.run_id = "12345"
        self.fixture.prepare()
        provenance_path = self.assets / "provenance.json"
        provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
        provenance["predicate"]["runDetails"] = {
            "builder": {
                "id": f"https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/{self.run_id}"
            },
            "metadata": {"invocationId": self.run_id},
        }
        provenance_path.write_text(json.dumps(provenance), encoding="utf-8")
        self.approval_dir = Path(self.fixture.temp.name) / "approval"
        self.record_path = self.approval_dir / "APPROVAL.json"
        MODULE.prepare(
            self.assets, self.version, self.commit, self.run_id, self.record_path,
            "wtfdelphia/tg-monitor-bot",
        )
        self.digest = hashlib.sha256(self.record_path.read_bytes()).hexdigest()
        (self.approval_dir / "APPROVAL.json.sha256").write_text(
            f"{self.digest}  APPROVAL.json\n", encoding="ascii"
        )

    def tearDown(self) -> None:
        self.fixture.tearDown()

    def verify(self, record: Path | None = None, digest: str | None = None) -> None:
        MODULE.verify(
            self.assets,
            record or self.record_path,
            digest or self.digest,
            self.version,
            self.commit,
            self.run_id,
            "wtfdelphia/tg-monitor-bot",
        )

    def test_approval_binds_exact_unsigned_assets(self) -> None:
        self.verify()

    def test_prepare_refuses_to_overwrite_approval_record(self) -> None:
        with self.assertRaises(ValueError):
            MODULE.prepare(
                self.assets, self.version, self.commit, self.run_id, self.record_path,
                "wtfdelphia/tg-monitor-bot",
            )

    def test_tampered_or_extra_asset_fails(self) -> None:
        source = self.assets / f"tgm-{self.version}-source.tar.gz"
        original = source.read_bytes()
        source.write_bytes(original + b"tampered")
        with self.assertRaises(ValueError):
            self.verify()
        source.write_bytes(original)
        (self.assets / "extra.txt").write_text("extra\n", encoding="utf-8")
        with self.assertRaises(ValueError):
            self.verify()

    def test_wrong_record_name_or_digest_fails(self) -> None:
        renamed = self.approval_dir / "OTHER.json"
        self.record_path.rename(renamed)
        with self.assertRaises(ValueError):
            self.verify(record=renamed)
        renamed.rename(self.record_path)
        with self.assertRaises(ValueError):
            self.verify(digest="0" * 64)

    def test_sidecar_must_match_exact_record_digest_and_name(self) -> None:
        sidecar = self.approval_dir / "APPROVAL.json.sha256"
        sidecar.write_text(f"{self.digest}  OTHER.json\n", encoding="ascii")
        with self.assertRaises(ValueError):
            self.verify()
        sidecar.write_text(f"{'0' * 64}  APPROVAL.json\n", encoding="ascii")
        with self.assertRaises(ValueError):
            self.verify()

    def test_approval_statement_binds_actor_run_and_source_record(self) -> None:
        output = self.approval_dir / "approval-statement.json"
        MODULE.approve(
            self.assets, self.record_path, self.digest, self.version, self.commit,
            self.run_id, "67890", "WTFGEDelphia", "wtfdelphia/tg-monitor-bot", output,
        )
        statement = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(statement["status"], "approved")
        self.assertEqual(statement["approver"], "WTFGEDelphia")
        self.assertEqual(statement["approval_run_id"], "67890")
        self.assertEqual(statement["approval_record_sha256"], self.digest)
        self.assertEqual(statement["source_commit"], self.commit)


if __name__ == "__main__":
    unittest.main()
