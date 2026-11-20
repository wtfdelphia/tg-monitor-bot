import importlib.util
import contextlib
import io
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
            name: {
                "status": "pass",
                "evidence_ref": REFERENCE,
                **({"measured_value": "4h", "measured_at": "2026-11-18"} if name == "rpo" else {}),
                **({"measured_value": "2h", "measured_at": "2026-11-18"} if name == "rto" else {}),
            }
            for name in MODULE.REQUIRED_WP8
        },
    }
    manifest["production"]["authorization"] = {
        "restore": "authorized",
        "alerting": "authorized",
        "tenant_purge": "authorized",
    }
    manifest["production"]["consent"].update({
        "artifact_version": "consent-1",
        "artifact_sha256": "d" * 64,
        "approval_ref": REFERENCE,
        "approved_by_ref": "legal-owner",
        "approved_at": "2026-11-18",
    })
    manifest["production"]["reporting"].update({
        "artifact_version": "reporting-1",
        "artifact_sha256": "e" * 64,
        "approval_ref": REFERENCE,
        "approved_by_ref": "legal-owner",
        "approved_at": "2026-11-18",
    })
    manifest["production"]["backup"].update({
        "coverage_ref": REFERENCE,
        "retention_ref": REFERENCE,
        "verification_ref": REFERENCE,
        "backup_completed_at": "2026-11-18",
        "backup_sha256": "f" * 64,
    })
    manifest["production"]["cross_replica_retention"].update({
        "systems": sorted(MODULE.REQUIRED_WP8_REPLICA_SYSTEMS),
        "inventory_ref": REFERENCE,
        "delete_boundary_ref": REFERENCE,
    })
    manifest["production"]["restore"].update({
        "restored_at": "2026-11-18",
        "duration": "2h",
        "roles": "pass",
        "rls": "pass",
        "row_counts": "pass",
        "cleanup": "pass",
    })
    manifest["production"]["alerts"].update({
        "receipt_ref": REFERENCE,
        "acknowledged_at": "2026-11-18",
    })
    manifest["production"]["oncall"].update({
        "schedule_ref": REFERENCE,
        "escalation_ref": REFERENCE,
        "rollback_owner_ref": "ops-owner",
    })
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
            "authorization_ref": REFERENCE,
            "authorized_at": "2026-11-18",
            "readonly_identity": True,
        },
        "customer_document": {
            "status": "pass",
            "version": "1.0",
            "sha256": "c" * 64,
            "legal_review_ref": REFERENCE,
            "distribution_authorization_ref": REFERENCE,
            "legal_reviewer_ref": "legal-owner",
            "authorized_by_ref": "release-owner",
            "legal_reviewed_at": "2026-11-18",
            "authorized_at": "2026-11-18",
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


def g3_manifest():
    return {
        "schema": "tgm/external-closure-evidence/v1",
        "kind": "g3",
        "baseline_commit": BASELINE,
        "recorded_at": "2026-11-18",
        "work_packages": {
            name: {"status": "pass", "evidence_ref": REFERENCE}
            for name in MODULE.REQUIRED_G3_WORK_PACKAGES
        },
        "exits": {
            name: {"status": "pass", "evidence_ref": REFERENCE}
            for name in MODULE.REQUIRED_G3_EXITS
        },
    }


class ExternalClosureEvidenceTests(unittest.TestCase):
    def write(self, directory, value, mode=0o600, name="evidence.json"):
        path = Path(directory) / name
        path.write_text(json.dumps(value), encoding="utf-8")
        os.chmod(path, mode)
        return path

    def test_complete_manifests_close_each_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            for kind, manifest in (
                ("g1", g1_manifest()),
                ("g3", g3_manifest()),
                ("wp8", wp8_manifest()),
                ("g4", g4_manifest()),
            ):
                result = MODULE.check_manifest(self.write(directory, manifest), kind)
                self.assertEqual(result["gate"], "pass")

    def test_g3_wp8_blocker_keeps_overall_gate_blocked(self):
        manifest = g3_manifest()
        manifest["work_packages"]["wp-8"] = {
            "status": "blocked",
            "reason": "生产恢复、跨副本留存、RPO/RTO、告警和值班证据缺失",
        }
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g3")
        self.assertEqual(result["gate"], "blocked")
        self.assertEqual(result["work_packages"]["wp-8"], "blocked")

    def test_g3_nonpass_item_requires_reason(self):
        manifest = g3_manifest()
        manifest["exits"]["end_to_end"] = {"status": "not-run"}
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g3")

    def test_g3_cli_requires_wp8_input(self):
        with tempfile.TemporaryDirectory() as directory:
            g3_path = self.write(directory, g3_manifest(), name="g3.json")
            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    MODULE.main(["--g3", str(g3_path)])
        self.assertEqual(error.exception.code, 2)

    def test_g3_cli_validates_wp8_input_and_preserves_blocked_state(self):
        g3 = g3_manifest()
        g3["work_packages"]["wp-8"] = {
            "status": "blocked",
            "reason": "生产证据待补齐",
        }
        wp8 = wp8_manifest()
        wp8["production"]["restore"] = {
            "status": "blocked",
            "reason": "尚无生产恢复记录",
        }
        wp8["production"]["authorization"]["restore"] = "not-authorized"
        with tempfile.TemporaryDirectory() as directory:
            g3_path = self.write(directory, g3, name="g3.json")
            wp8_path = self.write(directory, wp8, name="wp8.json")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                result = MODULE.main(["--g3", str(g3_path), "--wp8", str(wp8_path)])
        self.assertEqual(result, 2)
        report = json.loads(output.getvalue())
        self.assertEqual(report["gates"]["g3"]["work_packages"]["wp-8"], "blocked")
        self.assertEqual(report["gates"]["wp8"]["gate"], "blocked")

    def test_g3_cli_rejects_wp8_status_disagreement(self):
        g3 = g3_manifest()
        wp8 = wp8_manifest()
        wp8["production"]["restore"] = {
            "status": "blocked",
            "reason": "尚无生产恢复记录",
        }
        wp8["production"]["authorization"]["restore"] = "not-authorized"
        with tempfile.TemporaryDirectory() as directory:
            g3_path = self.write(directory, g3, name="g3.json")
            wp8_path = self.write(directory, wp8, name="wp8.json")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                result = MODULE.main(["--g3", str(g3_path), "--wp8", str(wp8_path)])
        self.assertEqual(result, 1)
        report = json.loads(output.getvalue())
        self.assertEqual(report["overall"], "error")
        self.assertIn("wp-8 声明 pass", report["errors"][0])

    def test_g1_without_provider_failure_evidence_is_blocked(self):
        manifest = g1_manifest()
        manifest["provider"]["unknown_kid"] = {
            "status": "not-run",
            "active_probe": False,
            "reason": "没有 provider 正式沙箱或批准隔离材料",
        }
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

    def test_g1_nonpass_provider_item_requires_reason(self):
        manifest = g1_manifest()
        manifest["provider"]["unknown_kid"] = {
            "status": "not-run",
            "active_probe": False,
        }
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
        manifest["flood_wait"]["reason"] = "自然事件尚未完成等待到期验证"
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g4")
        self.assertEqual(result["gate"], "blocked")
        self.assertEqual(result["flood_wait"], "partial/not-run")

    def test_g4_nonpass_items_require_reasons(self):
        manifest = g4_manifest()
        manifest["fingerprint_recompute"] = {
            "status": "partial",
            "reason": "生产范围尚未批准",
        }
        manifest["flood_wait"] = {
            "status": "not-run",
            "active_probe": False,
            "reason": "没有自然或 provider 隔离事件",
        }
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g4")
        self.assertEqual(result["gate"], "blocked")

        manifest["flood_wait"].pop("reason")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g4")

    def test_g4_flood_wait_requires_natural_observation(self):
        manifest = g4_manifest()
        manifest["flood_wait"]["source"] = "approved-isolation"
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g4")

    def test_g4_customer_document_requires_review_and_release_authority(self):
        manifest = g4_manifest()
        manifest["customer_document"] = {
            "status": "release-blocked",
            "reason": "法务审阅和正式分发授权仍未完成",
        }
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "g4")
        self.assertEqual(result["customer_document"], "release-blocked")
        self.assertEqual(result["gate"], "blocked")

        manifest["customer_document"].pop("reason")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "g4")

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

    def test_wp8_restore_cannot_pass_without_authorization(self):
        manifest = wp8_manifest()
        manifest["production"]["authorization"]["restore"] = "not-authorized"
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_rpo_rto_require_measured_values(self):
        manifest = wp8_manifest()
        del manifest["production"]["rpo"]["measured_value"]
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_rpo_rto_reject_non_measurement_duration_text(self):
        for field in ("rpo", "rto"):
            for measured_value in ("unknown", "not-measured", "-1h", "2 hours", "", "1h later"):
                manifest = wp8_manifest()
                manifest["production"][field]["measured_value"] = measured_value
                with self.subTest(field=field, measured_value=measured_value):
                    with tempfile.TemporaryDirectory() as directory:
                        with self.assertRaises(MODULE.ManifestError):
                            MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_rpo_rto_accept_nonnegative_duration_units(self):
        manifest = wp8_manifest()
        manifest["production"]["rpo"]["measured_value"] = "0s"
        manifest["production"]["rto"]["measured_value"] = "0.5h"
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "wp8")
        self.assertEqual(result["gate"], "pass")

    def test_wp8_replica_scope_requires_all_copy_systems(self):
        manifest = wp8_manifest()
        systems = manifest["production"]["cross_replica_retention"]["systems"]
        systems.remove("logs")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_replica_scope_rejects_malformed_system_entries(self):
        for invalid_entry in ({"name": "logs"}, 7, "", None, []):
            manifest = wp8_manifest()
            manifest["production"]["cross_replica_retention"]["systems"].append(invalid_entry)
            with self.subTest(invalid_entry=invalid_entry):
                with tempfile.TemporaryDirectory() as directory:
                    with self.assertRaises(MODULE.ManifestError):
                        MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_restore_requires_runtime_checks(self):
        manifest = wp8_manifest()
        manifest["production"]["restore"]["rls"] = "not-run"
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "wp8")

    def test_wp8_nonpass_items_require_reasons(self):
        manifest = wp8_manifest()
        manifest["production"]["restore"] = {
            "status": "blocked",
            "reason": "生产恢复窗口尚未批准",
        }
        with tempfile.TemporaryDirectory() as directory:
            result = MODULE.check_manifest(self.write(directory, manifest), "wp8")
        self.assertEqual(result["gate"], "blocked")

        manifest["production"]["backup"] = {"status": "not-run"}
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ManifestError):
                MODULE.check_manifest(self.write(directory, manifest), "wp8")

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
