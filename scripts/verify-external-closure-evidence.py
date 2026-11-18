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
    if status(item.get("status"), label + ".status") != "pass":
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
        passed = status(item.get("status"), "production." + name + ".status") == "pass"
        if passed:
            evidence_ref(item.get("evidence_ref"), "production." + name + ".evidence_ref")
        results[name] = passed
    authorization = production.get("authorization")
    if not isinstance(authorization, dict):
        raise ManifestError("production.authorization 必须是 JSON object")
    for name in ("restore", "alerting", "tenant_purge"):
        value = status(authorization.get(name), "production.authorization." + name)
        if value not in ("authorized", "not-authorized"):
            raise ManifestError("production.authorization." + name + " 状态无效")
    return {
        "kind": "wp8",
        "baseline_commit": baseline,
        "gate": "pass" if all(results.values()) else "blocked",
        "production": results,
        "authorization": {name: authorization[name] for name in ("restore", "alerting", "tenant_purge")},
    }


def check_g4(manifest):
    baseline = check_common(manifest, "g4")
    fingerprint = manifest.get("fingerprint_recompute")
    if not isinstance(fingerprint, dict):
        raise ManifestError("fingerprint_recompute 必须是 JSON object")
    fingerprint_pass = status(fingerprint.get("status"), "fingerprint_recompute.status") == "pass"
    if fingerprint_pass:
        evidence_ref(fingerprint.get("evidence_ref"), "fingerprint_recompute.evidence_ref")
        nonempty_string(fingerprint.get("scope_ref"), "fingerprint_recompute.scope_ref")
        evidence_ref(fingerprint.get("result_ref"), "fingerprint_recompute.result_ref")
        if fingerprint.get("authorization_status") != "approved":
            raise ManifestError("fingerprint_recompute.authorization_status 必须为 approved")
        if fingerprint.get("readonly_identity") is not True:
            raise ManifestError("fingerprint_recompute.readonly_identity 必须为 true")
    flood = manifest.get("flood_wait")
    if not isinstance(flood, dict):
        raise ManifestError("flood_wait 必须是 JSON object")
    if flood.get("active_probe") is not False:
        raise ManifestError("flood_wait.active_probe 必须明确为 false")
    flood_pass = status(flood.get("status"), "flood_wait.status") == "pass"
    if flood_pass:
        source = nonempty_string(flood.get("source"), "flood_wait.source")
        if source not in ALLOWED_SOURCES:
            raise ManifestError("flood_wait.source 不在允许的证据来源集合")
        evidence_ref(flood.get("evidence_ref"), "flood_wait.evidence_ref")
        check_date(flood.get("observed_at"), "flood_wait.observed_at")
        loop = flood.get("closed_loop")
        if not isinstance(loop, dict):
            raise ManifestError("flood_wait.closed_loop 必须是 JSON object")
        if any(loop.get(name) != "pass" for name in REQUIRED_G4_LOOP):
            flood_pass = False
    return {
        "kind": "g4",
        "baseline_commit": baseline,
        "gate": "pass" if fingerprint_pass and flood_pass else "blocked",
        "fingerprint_recompute": "pass" if fingerprint_pass else "partial",
        "flood_wait": "pass" if flood_pass else "partial/not-run",
    }


def check_manifest(path, kind):
    manifest = private_json(path)
    if kind == "g1":
        return check_g1(manifest)
    if kind == "wp8":
        return check_wp8(manifest)
    if kind == "g4":
        return check_g4(manifest)
    raise ManifestError("未知证据类型")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--g1", type=Path)
    parser.add_argument("--wp8", type=Path)
    parser.add_argument("--g4", type=Path)
    args = parser.parse_args(argv)
    paths = (("g1", args.g1), ("wp8", args.wp8), ("g4", args.g4))
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
    result["overall"] = "error" if malformed else (
        "pass" if result["gates"] and all(value["gate"] == "pass" for value in result["gates"].values()) else "blocked"
    )
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 1 if malformed else (0 if result["overall"] == "pass" else 2)


if __name__ == "__main__":
    sys.exit(main())
