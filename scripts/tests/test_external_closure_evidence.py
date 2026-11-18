import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "verify-external-closure-evidence.py"
SPEC = importlib.util.spec_from_file_location("closure_evidence", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


BASELINE = "a" * 40
REFERENCE = "ticket-2026-11-18-001"


def item(source="approved-isolation"):
    return {
        "status": "pass",
        "source": source,
        "evidence_ref": REFERENCE,
        "observed_at": "2026-11-18",
        "active_probe": False,
    }


def g1_manifest():
    return {
        "schema": "tgm/external-closure-evidence/v1",
        "kind": "g1",
        "baseline_commit": BASELINE,
        "recorded_at": "2026-11-18",
        "provider": {name: item() for name in MODULE.REQUIRED_G1_PROVIDER},
        "tenant_api_id_review": {"status": "pass", "evidence_ref": REFERENCE},
        "formal_signoff": {
            "status": "signed",
            "signer_ref": "owner-g1",
            "signed_at": "2026-11-18",
            "evidence_ref": REFERENCE,
            "decision": "go",
        },
        "srs_v1_1": {"status": "frozen", "version": "1.1", "sha256": ["b" * 64]},
    }


def wp8_manifest():
    manifest = {
        "schema": "tgm/external-closure-evidence/v1",
        "kind": "wp8",
        "baseline_commit": BASELINE,
        "recorded_at": "2026-11-18",
        "production": {
            name: {"status": "pass", "evidence_ref": REFERENCE}
            for name in MODULE.REQUIRED_WP8
        },
    }
    manifest["production"]["authorization"] = {
        "restore": "authorized",
        "alerting": "authorized",
        "tenant_purge": "authorized",
    }
    return manifest


def g4_manifest():
    return {
        "schema": "tgm/external-closure-evidence/v1",
        "kind": "g4",
        "baseline_commit": BASELINE,
        "recorded_at": "2026-11-18",
        "fingerprint_recompute": {
            "status": "pass",
            "scope_ref": REFERENCE,
            "evidence_ref": REFERENCE,
            "result_ref": REFERENCE,
            "authorization_status": "approved",
            "readonly_identity": True,
        },
        "flood_wait": {
            "status": "pass",
            "source": "natural-observation",
            "evidence_ref": REFERENCE,
            "observed_at": "2026-11-18",
            "active_probe": False,
            "closed_loop": {name: "pass" for name in MODULE.REQUIRED_G4_LOOP},
        },
    }


class ExternalClosureEvidenceTests(unittest.TestCase):
    def write(self, directory, value, mode=0o600):
        path = Path(directory) / "evidence.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        os.chmod(path, mode)
        return path

    def test_complete_manifests_close_each_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            for kind, manifest in (("g1", g1_manifest()), ("wp8", wp8_manifest()), ("g4", g4_manifest())):
                result = MODULE.check_manifest(self.write(directory, manifest), kind)
                self.assertEqual(result["gate"], "pass")

    def test_g1_without_provider_failure_evidence_is_blocked(self):
        manifest = g1_manifest()
        manifest["provider"]["unknown_kid"]["status"] = "not-run"
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g1")
        self.assertEqual(result["gate"], "blocked")
        self.assertEqual(result["formal_signoff"], "signed")

    def test_g1_not_run_provider_item_rejects_active_probe(self):
        manifest = g1_manifest()
        manifest["provider"]["unknown_kid"]["status"] = "not-run"
        manifest["provider"]["unknown_kid"]["active_probe"] = True
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g1")

    def test_g4_active_probe_is_rejected(self):
        manifest = g4_manifest()
        manifest["flood_wait"]["active_probe"] = True
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g4")

    def test_g4_incomplete_closed_loop_is_blocked(self):
        manifest = g4_manifest()
        manifest["flood_wait"]["closed_loop"]["wait_expiry"] = "not-run"
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g4")
        self.assertEqual(result["gate"], "blocked")

    def test_g4_not_run_rejects_active_probe(self):
        manifest = g4_manifest()
        manifest["flood_wait"]["status"] = "not-run"
        manifest["flood_wait"]["active_probe"] = True
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g4")

    def test_wp8_without_purge_authorization_keeps_purge_separate(self):
        manifest = wp8_manifest()
        manifest["production"]["authorization"]["tenant_purge"] = "not-authorized"
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "wp8")
        self.assertEqual(result["gate"], "pass")
        self.assertEqual(result["authorization"]["tenant_purge"], "not-authorized")

    def test_public_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.write(directory, g1_manifest(), 0o644)
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(path, "g1")

    def test_secret_like_reference_is_rejected(self):
        manifest = g1_manifest()
        manifest["formal_signoff"]["evidence_ref"] = "session-ticket-1"
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g1")


if __name__ == "__main__":
    unittest.main()
