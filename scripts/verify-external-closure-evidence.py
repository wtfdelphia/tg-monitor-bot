#!/usr/bin/env python3
"""Validate redacted closure manifests without executing or echoing secrets.

The checker validates structure and recorded status only; it cannot establish that an
external evidence reference is authentic or that a production action occurred.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import stat
import sys


HEX_COMMIT = re.compile(r"^[0-9a-f]{40}$")
HEX_SHA256 = re.compile(r"^[0-9a-f]{64}$")
DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
DURATION = re.compile(r"^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:ms|s|m|h|d)$")
ALLOWED_SOURCES = {"provider-sandbox", "approved-isolation", "natural-observation"}
REQUIRED_G1_PROVIDER = ("rotation", "unknown_kid", "jwks_failure", "expired_token")
REQUIRED_G4_LOOP = (
    "retry_at_persisted",
    "identity_state",
    "retry_suppressed",
    "wait_expiry",
    "single_delivery_after_recovery",
    "duplicate_delivery",
    "journal_redaction",
    "cleanup",
)
REQUIRED_G4_CUSTOMER_DOCS = (
    "legal_review",
    "distribution_authorization",
)
REQUIRED_WP8_REPLICA_SYSTEMS = {
    "postgres_backup",
    "wal_archive",
    "restore_replica",
    "object_storage",
    "redis",
    "logs",
    "telegram_external_copy",
}
REQUIRED_WP8 = (
    "consent",
    "reporting",
    "backup",
    "cross_replica_retention",
    "restore",
    "rpo",
    "rto",
    "alerts",
    "oncall",
)
REQUIRED_G3_WORK_PACKAGES = tuple("wp-" + str(number) for number in range(1, 9))
REQUIRED_G3_EXITS = ("end_to_end", "rls_explain", "pr_gate_window")
GATE_STATUSES = {"pass", "blocked", "partial", "not-run", "fail", "release-blocked"}


class ManifestError(ValueError):
    pass


def private_json(path):
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ManifestError("证据文件不存在或不可读") from error
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ManifestError("证据文件必须是当前用户拥有的普通文件")
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        raise ManifestError("证据文件必须使用 0600 或更严格权限")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError) as error:
        raise ManifestError("证据文件不是合法 JSON") from error
    if not isinstance(value, dict):
        raise ManifestError("证据文件根节点必须是 JSON object")
    return value


def nonempty_string(value, label):
    if not isinstance(value, str) or not value.strip() or value.startswith("REPLACE"):
        raise ManifestError(label + " 缺失或仍为占位值")
    return value.strip()


def status(value, label):
    return nonempty_string(value, label)


def check_gate_status(value, label):
    value = status(value, label)
    if value not in GATE_STATUSES:
        raise ManifestError(label + " 状态无效")
    return value


def check_nonpass_reason(item, value, label):
    if value == "pass":
        return
    if not isinstance(item, dict):
        raise ManifestError(label + " 必须是 JSON object")
    nonempty_string(item.get("reason"), label + ".reason")


def evidence_ref(value, label):
    reference = nonempty_string(value, label)
    if any(secret in reference.lower() for secret in ("token", "secret", "password", "api_hash", "session")):
        raise ManifestError(label + " 不得包含凭据标识")
    return reference


def check_date(value, label):
    date_value = nonempty_string(value, label)
    if not DATE.fullmatch(date_value):
        raise ManifestError(label + " 必须是 YYYY-MM-DD")
    try:
        datetime.datetime.strptime(date_value, "%Y-%m-%d").date()
    except ValueError as error:
        raise ManifestError(label + " 不是有效日期") from error
    return date_value


def check_common(manifest, kind):
    if manifest.get("schema") != "tgm/external-closure-evidence/v1":
        raise ManifestError("schema 不受支持")
    if manifest.get("kind") != kind:
        raise ManifestError("证据类型与参数不一致")
    baseline = nonempty_string(manifest.get("baseline_commit"), "baseline_commit")
    if not HEX_COMMIT.fullmatch(baseline):
        raise ManifestError("baseline_commit 必须是 40 位小写 SHA-1")
    check_date(manifest.get("recorded_at"), "recorded_at")
    return baseline


def check_provider_item(item, label):
    if not isinstance(item, dict):
        raise ManifestError(label + " 必须是 JSON object")
    if item.get("active_probe") is not False:
        raise ManifestError(label + ".active_probe 必须明确为 false")
    value = check_gate_status(item.get("status"), label + ".status")
    if value != "pass":
        check_nonpass_reason(item, value, label)
        return False
    source = nonempty_string(item.get("source"), label + ".source")
    if source not in ALLOWED_SOURCES:
        raise ManifestError(label + ".source 不在允许的证据来源集合")
    evidence_ref(item.get("evidence_ref"), label + ".evidence_ref")
    check_date(item.get("observed_at"), label + ".observed_at")
    return True


def check_g1(manifest):
    baseline = check_common(manifest, "g1")
    provider = manifest.get("provider")
    if not isinstance(provider, dict):
        raise ManifestError("provider 必须是 JSON object")
    provider_results = {}
    for name in REQUIRED_G1_PROVIDER:
        provider_results[name] = check_provider_item(provider.get(name), "provider." + name)

    tenant = manifest.get("tenant_api_id_review")
    if not isinstance(tenant, dict) or status(tenant.get("status"), "tenant_api_id_review.status") != "pass":
        raise ManifestError("tenant_api_id_review 必须为 pass")
    evidence_ref(tenant.get("evidence_ref"), "tenant_api_id_review.evidence_ref")

    signoff = manifest.get("formal_signoff")
    if not isinstance(signoff, dict):
        raise ManifestError("formal_signoff 必须是 JSON object")
    signed = status(signoff.get("status"), "formal_signoff.status") == "signed"
    decision = None
    if signed:
        nonempty_string(signoff.get("signer_ref"), "formal_signoff.signer_ref")
        check_date(signoff.get("signed_at"), "formal_signoff.signed_at")
        evidence_ref(signoff.get("evidence_ref"), "formal_signoff.evidence_ref")
        decision = nonempty_string(signoff.get("decision"), "formal_signoff.decision")
        if decision not in ("go", "no-go"):
            raise ManifestError("formal_signoff.decision 必须是 go 或 no-go")
    else:
        signed = False

    srs = manifest.get("srs_v1_1")
    if not isinstance(srs, dict):
        raise ManifestError("srs_v1_1 必须是 JSON object")
    frozen = status(srs.get("status"), "srs_v1_1.status") == "frozen"
    if frozen:
        if srs.get("version") != "1.1":
            raise ManifestError("srs_v1_1.version 必须为 1.1")
        hashes = srs.get("sha256")
        if not isinstance(hashes, list) or not hashes or any(not isinstance(value, str) or not HEX_SHA256.fullmatch(value) for value in hashes):
            raise ManifestError("srs_v1_1.sha256 必须是非空 SHA-256 列表")

    gate = all(provider_results.values()) and signed and decision == "go" and frozen
    return {
        "kind": "g1",
        "baseline_commit": baseline,
        "gate": "pass" if gate else "blocked",
        "provider": provider_results,
        "formal_signoff": "signed" if signed else "pending",
        "decision": decision or "pending",
        "srs_v1_1": "frozen" if frozen else "prepared-not-frozen",
    }


def check_wp8(manifest):
    baseline = check_common(manifest, "wp8")
    production = manifest.get("production")
    if not isinstance(production, dict):
        raise ManifestError("production 必须是 JSON object")
    results = {}
    for name in REQUIRED_WP8:
        item = production.get(name)
        if not isinstance(item, dict):
            raise ManifestError("production." + name + " 必须是 JSON object")
        value = check_gate_status(item.get("status"), "production." + name + ".status")
        passed = value == "pass"
        if passed:
            evidence_ref(item.get("evidence_ref"), "production." + name + ".evidence_ref")
            if name in ("consent", "reporting"):
                nonempty_string(item.get("artifact_version"), "production." + name + ".artifact_version")
                artifact_hash = nonempty_string(item.get("artifact_sha256"), "production." + name + ".artifact_sha256")
                if not HEX_SHA256.fullmatch(artifact_hash):
                    raise ManifestError("production." + name + ".artifact_sha256 必须是 64 位小写 SHA-256")
                evidence_ref(item.get("approval_ref"), "production." + name + ".approval_ref")
                evidence_ref(item.get("approved_by_ref"), "production." + name + ".approved_by_ref")
                check_date(item.get("approved_at"), "production." + name + ".approved_at")
            elif name == "backup":
                for field in ("coverage_ref", "retention_ref", "verification_ref"):
                    evidence_ref(item.get(field), "production.backup." + field)
                check_date(item.get("backup_completed_at"), "production.backup.backup_completed_at")
                backup_hash = nonempty_string(item.get("backup_sha256"), "production.backup.backup_sha256")
                if not HEX_SHA256.fullmatch(backup_hash):
                    raise ManifestError("production.backup.backup_sha256 必须是 64 位小写 SHA-256")
            elif name == "cross_replica_retention":
                systems = item.get("systems")
                if not isinstance(systems, list):
                    raise ManifestError("production.cross_replica_retention.systems 必须是字符串数组")
                system_names = {
                    nonempty_string(system, "production.cross_replica_retention.systems[" + str(index) + "]")
                    for index, system in enumerate(systems)
                }
                if not REQUIRED_WP8_REPLICA_SYSTEMS.issubset(system_names):
                    raise ManifestError("production.cross_replica_retention.systems 未覆盖全部副本系统")
                evidence_ref(item.get("inventory_ref"), "production.cross_replica_retention.inventory_ref")
                evidence_ref(item.get("delete_boundary_ref"), "production.cross_replica_retention.delete_boundary_ref")
            elif name == "restore":
                check_date(item.get("restored_at"), "production.restore.restored_at")
                nonempty_string(item.get("duration"), "production.restore.duration")
                for field in ("roles", "rls", "row_counts", "cleanup"):
                    if item.get(field) != "pass":
                        raise ManifestError("production.restore." + field + " 必须是 pass")
            elif name == "rpo" or name == "rto":
                measured_value = nonempty_string(item.get("measured_value"), "production." + name + ".measured_value")
                if not DURATION.fullmatch(measured_value):
                    raise ManifestError("production." + name + ".measured_value 必须是非负数值加单位（ms/s/m/h/d）")
                check_date(item.get("measured_at"), "production." + name + ".measured_at")
            elif name == "alerts":
                evidence_ref(item.get("receipt_ref"), "production.alerts.receipt_ref")
                check_date(item.get("acknowledged_at"), "production.alerts.acknowledged_at")
            elif name == "oncall":
                evidence_ref(item.get("schedule_ref"), "production.oncall.schedule_ref")
                evidence_ref(item.get("escalation_ref"), "production.oncall.escalation_ref")
                evidence_ref(item.get("rollback_owner_ref"), "production.oncall.rollback_owner_ref")
        else:
            check_nonpass_reason(item, value, "production." + name)
        results[name] = value
    authorization = production.get("authorization")
    if not isinstance(authorization, dict):
        raise ManifestError("production.authorization 必须是 JSON object")
    for name in ("restore", "alerting", "tenant_purge"):
        value = status(authorization.get(name), "production.authorization." + name)
        if value not in ("authorized", "not-authorized"):
            raise ManifestError("production.authorization." + name + " 状态无效")
    if results["restore"] == "pass" and authorization["restore"] != "authorized":
        raise ManifestError("production.restore 通过时必须已获 restore 授权")
    if results["alerts"] == "pass" and authorization["alerting"] != "authorized":
        raise ManifestError("production.alerts 通过时必须已获 alerting 授权")
    return {
        "kind": "wp8",
        "baseline_commit": baseline,
        "gate": "pass" if all(value == "pass" for value in results.values()) else "blocked",
        "production": results,
        "authorization": {name: authorization[name] for name in ("restore", "alerting", "tenant_purge")},
    }


def check_g4(manifest):
    baseline = check_common(manifest, "g4")
    fingerprint = manifest.get("fingerprint_recompute")
    if not isinstance(fingerprint, dict):
        raise ManifestError("fingerprint_recompute 必须是 JSON object")
    fingerprint_status = check_gate_status(fingerprint.get("status"), "fingerprint_recompute.status")
    fingerprint_pass = fingerprint_status == "pass"
    if fingerprint_pass:
        evidence_ref(fingerprint.get("evidence_ref"), "fingerprint_recompute.evidence_ref")
        evidence_ref(fingerprint.get("scope_ref"), "fingerprint_recompute.scope_ref")
        evidence_ref(fingerprint.get("result_ref"), "fingerprint_recompute.result_ref")
        if fingerprint.get("authorization_status") != "approved":
            raise ManifestError("fingerprint_recompute.authorization_status 必须为 approved")
        evidence_ref(fingerprint.get("authorization_ref"), "fingerprint_recompute.authorization_ref")
        check_date(fingerprint.get("authorized_at"), "fingerprint_recompute.authorized_at")
        if fingerprint.get("readonly_identity") is not True:
            raise ManifestError("fingerprint_recompute.readonly_identity 必须为 true")
    else:
        check_nonpass_reason(fingerprint, fingerprint_status, "fingerprint_recompute")

    customer_docs = manifest.get("customer_document")
    if not isinstance(customer_docs, dict):
        raise ManifestError("customer_document 必须是 JSON object")
    customer_docs_status = check_gate_status(customer_docs.get("status"), "customer_document.status")
    if customer_docs_status == "pass":
        for name in REQUIRED_G4_CUSTOMER_DOCS:
            evidence_ref(customer_docs.get(name + "_ref"), "customer_document." + name + "_ref")
        nonempty_string(customer_docs.get("version"), "customer_document.version")
        nonempty_string(customer_docs.get("sha256"), "customer_document.sha256")
        if not HEX_SHA256.fullmatch(customer_docs["sha256"]):
            raise ManifestError("customer_document.sha256 必须是 64 位小写 SHA-256")
        evidence_ref(customer_docs.get("legal_reviewer_ref"), "customer_document.legal_reviewer_ref")
        evidence_ref(customer_docs.get("authorized_by_ref"), "customer_document.authorized_by_ref")
        check_date(customer_docs.get("legal_reviewed_at"), "customer_document.legal_reviewed_at")
        check_date(customer_docs.get("authorized_at"), "customer_document.authorized_at")
    else:
        check_nonpass_reason(customer_docs, customer_docs_status, "customer_document")

    flood = manifest.get("flood_wait")
    if not isinstance(flood, dict):
        raise ManifestError("flood_wait 必须是 JSON object")
    if flood.get("active_probe") is not False:
        raise ManifestError("flood_wait.active_probe 必须明确为 false")
    flood_status = check_gate_status(flood.get("status"), "flood_wait.status")
    flood_pass = flood_status == "pass"
    if flood_pass:
        source = nonempty_string(flood.get("source"), "flood_wait.source")
        if source != "natural-observation":
            raise ManifestError("flood_wait.source 必须是 natural-observation")
        evidence_ref(flood.get("evidence_ref"), "flood_wait.evidence_ref")
        check_date(flood.get("observed_at"), "flood_wait.observed_at")
        loop = flood.get("closed_loop")
        if not isinstance(loop, dict):
            raise ManifestError("flood_wait.closed_loop 必须是 JSON object")
        loop_results = {
            name: check_gate_status(loop.get(name), "flood_wait.closed_loop." + name)
            for name in REQUIRED_G4_LOOP
        }
        if any(value != "pass" for value in loop_results.values()):
            nonempty_string(flood.get("reason"), "flood_wait.reason")
            flood_pass = False
    else:
        check_nonpass_reason(flood, flood_status, "flood_wait")
    flood_result = "pass" if flood_pass else (
        "partial/not-run" if flood_status == "not-run" or flood_status == "pass" else flood_status
    )
    return {
        "kind": "g4",
        "baseline_commit": baseline,
        "gate": "pass" if fingerprint_pass and flood_pass and customer_docs_status == "pass" else "blocked",
        "fingerprint_recompute": fingerprint_status,
        "customer_document": customer_docs_status,
        "flood_wait": flood_result,
    }


def check_g3_item(item, label):
    if not isinstance(item, dict):
        raise ManifestError(label + " 必须是 JSON object")
    value = check_gate_status(item.get("status"), label + ".status")
    if value == "pass":
        evidence_ref(item.get("evidence_ref"), label + ".evidence_ref")
    else:
        check_nonpass_reason(item, value, label)
    return value


def check_g3(manifest):
    baseline = check_common(manifest, "g3")
    work_packages = manifest.get("work_packages")
    if not isinstance(work_packages, dict):
        raise ManifestError("work_packages 必须是 JSON object")
    package_results = {}
    for name in REQUIRED_G3_WORK_PACKAGES:
        package_results[name] = check_g3_item(work_packages.get(name), "work_packages." + name)

    exits = manifest.get("exits")
    if not isinstance(exits, dict):
        raise ManifestError("exits 必须是 JSON object")
    exit_results = {}
    for name in REQUIRED_G3_EXITS:
        exit_results[name] = check_g3_item(exits.get(name), "exits." + name)

    gate = all(value == "pass" for value in package_results.values()) and all(
        value == "pass" for value in exit_results.values()
    )
    return {
        "kind": "g3",
        "baseline_commit": baseline,
        "gate": "pass" if gate else "blocked",
        "work_packages": package_results,
        "exits": exit_results,
    }


def check_manifest(path, kind):
    manifest = private_json(path)
    if kind == "g1":
        return check_g1(manifest)
    if kind == "wp8":
        return check_wp8(manifest)
    if kind == "g4":
        return check_g4(manifest)
    if kind == "g3":
        return check_g3(manifest)
    raise ManifestError("未知证据类型")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--g1", type=Path)
    parser.add_argument("--g3", type=Path)
    parser.add_argument("--wp8", type=Path)
    parser.add_argument("--g4", type=Path)
    args = parser.parse_args(argv)
    if args.g3 is not None and args.wp8 is None:
        parser.error("G3 总体出口必须同时提供 --wp8，避免绕过 WP-8 子项")
    paths = (("g1", args.g1), ("g3", args.g3), ("wp8", args.wp8), ("g4", args.g4))
    if not any(path is not None for _, path in paths):
        parser.error("至少提供一个证据文件")
    result = {"gates": {}, "errors": []}
    malformed = False
    for kind, path in paths:
        if path is None:
            continue
        try:
            result["gates"][kind] = check_manifest(path, kind)
        except ManifestError as error:
            malformed = True
            result["errors"].append(kind + ": " + str(error))
    if args.g3 is not None and args.wp8 is not None:
        g3_result = result["gates"].get("g3")
        wp8_result = result["gates"].get("wp8")
        if g3_result is not None and wp8_result is not None:
            g3_wp8 = g3_result["work_packages"]["wp-8"]
            if g3_wp8 == "pass" and wp8_result["gate"] != "pass":
                malformed = True
                result["errors"].append("g3: wp-8 声明 pass，但 WP-8 证据门未通过")
            elif g3_wp8 != "pass" and wp8_result["gate"] == "pass":
                result["errors"].append("g3: WP-8 证据已 pass，但 G3 汇总记录尚未更新")
    result["overall"] = "error" if malformed else (
        "pass" if result["gates"] and all(value["gate"] == "pass" for value in result["gates"].values()) else "blocked"
    )
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 1 if malformed else (0 if result["overall"] == "pass" else 2)


if __name__ == "__main__":
    sys.exit(main())
