#!/usr/bin/env python3
"""Create or verify a stable-release approval record bound to exact assets."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("stable_release_assets", ROOT / "scripts/stable-release-assets.py")
ASSETS = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(ASSETS)


def fail(message: str) -> None:
    raise ValueError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def prepare(assets: Path, version: str, source_commit: str, prepare_run_id: str,
            output: Path, repository: str) -> dict[str, object]:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        fail("version must be MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        fail("source commit must be a full lowercase SHA-1")
    if not re.fullmatch(r"[1-9][0-9]*", prepare_run_id):
        fail("prepare run ID must be a positive integer")
    ASSETS.verify(type("Args", (), {
        "assets": assets,
        "version": version,
        "source_commit": source_commit,
        "repository": repository,
        "expected_run_id": prepare_run_id,
        "verify_signatures": False,
    })())
    expected_asset_names = {
        *ASSETS.expected_payloads(assets, version),
        "SHA256SUMS",
        "provenance.json",
    }
    names = sorted(path.name for path in assets.iterdir())
    if set(names) != expected_asset_names:
        fail("unsigned release asset inventory is not exact")
    if any(not (assets / name).is_file() or (assets / name).is_symlink() for name in names):
        fail("release asset is not a regular file")
    record: dict[str, object] = {
        "schema_version": 1,
        "status": "awaiting-explicit-release-approval",
        "repository": repository,
        "release_tag": "v" + version,
        "version": version,
        "source_commit": source_commit,
        "prepare_run_id": prepare_run_id,
        "source_manifest_sha256": sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt"),
        "assets": {name: sha256(assets / name) for name in names},
    }
    if output.exists() or output.is_symlink():
        fail("approval record output already exists")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(record, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print("approval_record_sha256=" + sha256(output))
    return record


def verify(assets: Path, record_path: Path, expected_record_sha256: str,
           version: str, source_commit: str, prepare_run_id: str,
           repository: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{64}", expected_record_sha256):
        fail("approval record SHA-256 must be 64 lowercase hex characters")
    if record_path.name != "APPROVAL.json" or not record_path.is_file() or record_path.is_symlink():
        fail("approval record must be a regular APPROVAL.json file")
    if sha256(record_path) != expected_record_sha256:
        fail("approval record SHA-256 mismatch")
    checksum_path = record_path.with_name(record_path.name + ".sha256")
    if not checksum_path.is_file() or checksum_path.is_symlink():
        fail("approval record checksum sidecar must be a regular file")
    checksum_lines = checksum_path.read_text(encoding="ascii").splitlines()
    if checksum_lines != [expected_record_sha256 + "  APPROVAL.json"]:
        fail("approval record checksum sidecar does not match the supplied digest")
    record = json.loads(record_path.read_text(encoding="utf-8"))
    if not isinstance(record, dict):
        fail("approval record root must be an object")
    expected_identity = {
        "schema_version": 1,
        "status": "awaiting-explicit-release-approval",
        "repository": repository,
        "release_tag": "v" + version,
        "version": version,
        "source_commit": source_commit,
        "prepare_run_id": prepare_run_id,
    }
    if any(record.get(key) != value for key, value in expected_identity.items()):
        fail("approval record identity does not match requested release")
    if set(record) != {*expected_identity, "source_manifest_sha256", "assets"}:
        fail("approval record contains unexpected or missing fields")
    payload_names = sorted(path.name for path in assets.iterdir())
    expected_asset_names = {
        *ASSETS.expected_payloads(assets, version),
        "SHA256SUMS",
        "provenance.json",
    }
    signature_names = {"SHA256SUMS.sigstore.json", "provenance.sigstore.json"}
    actual_names = set(payload_names)
    if (not expected_asset_names.issubset(actual_names)
            or actual_names - expected_asset_names not in (set(), signature_names)):
        fail("prepared unsigned release asset inventory is not exact")
    for name in payload_names:
        path = assets / name
        if not path.is_file() or path.is_symlink():
            fail("prepared asset is not a regular file: " + name)
    if record.get("assets") != {
        name: sha256(assets / name) for name in sorted(expected_asset_names)
    }:
        fail("prepared assets differ from approved SHA-256 inventory")
    manifest_digest = sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")
    if record.get("source_manifest_sha256") != manifest_digest:
        fail("approved source manifest digest mismatch")
    with tempfile.TemporaryDirectory(prefix="tgm-release-approved-assets-") as temporary:
        verified_assets = Path(temporary)
        for name in sorted(expected_asset_names):
            source = assets / name
            if not source.is_file() or source.is_symlink():
                fail("prepared payload is not a regular file: " + name)
            shutil.copyfile(source, verified_assets / name)
        ASSETS.verify(type("Args", (), {
            "assets": verified_assets,
            "version": version,
            "source_commit": source_commit,
            "repository": repository,
            "expected_run_id": prepare_run_id,
            "verify_signatures": False,
        })())
    print("release_approval=verified tag=v" + version + " source_commit=" + source_commit)


def approve(assets: Path, record_path: Path, expected_record_sha256: str,
            version: str, source_commit: str, prepare_run_id: str,
            approval_run_id: str, approver: str, repository: str,
            output: Path) -> None:
    if not re.fullmatch(r"[1-9][0-9]*", approval_run_id):
        fail("approval run ID must be a positive integer")
    if not re.fullmatch(r"[A-Za-z0-9-]+", approver):
        fail("approver must be a GitHub login")
    verify(assets, record_path, expected_record_sha256, version, source_commit,
           prepare_run_id, repository)
    statement = {
        "schema_version": 1,
        "status": "approved",
        "repository": repository,
        "release_tag": "v" + version,
        "version": version,
        "source_commit": source_commit,
        "prepare_run_id": prepare_run_id,
        "approval_record_sha256": expected_record_sha256,
        "approval_run_id": approval_run_id,
        "approver": approver,
        "approval_ref": "refs/tags/v" + version,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(statement, ensure_ascii=False, indent=2) + "\n"
    if output.exists():
        if output.read_text(encoding="utf-8") != encoded:
            fail("approval statement output already exists with different content")
    else:
        output.write_text(encoded, encoding="utf-8")
    print("approval_statement_sha256=" + sha256(output))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    create = subparsers.add_parser("prepare")
    create.add_argument("--assets", type=Path, required=True)
    create.add_argument("--version", required=True)
    create.add_argument("--source-commit", required=True)
    create.add_argument("--prepare-run-id", required=True)
    create.add_argument("--repository", default="wtfdelphia/tg-monitor-bot")
    create.add_argument("--output", type=Path, required=True)
    check = subparsers.add_parser("verify")
    check.add_argument("--assets", type=Path, required=True)
    check.add_argument("--record", type=Path, required=True)
    check.add_argument("--record-sha256", required=True)
    check.add_argument("--version", required=True)
    check.add_argument("--source-commit", required=True)
    check.add_argument("--prepare-run-id", required=True)
    check.add_argument("--repository", default="wtfdelphia/tg-monitor-bot")
    approve_parser = subparsers.add_parser("approve")
    approve_parser.add_argument("--assets", type=Path, required=True)
    approve_parser.add_argument("--record", type=Path, required=True)
    approve_parser.add_argument("--record-sha256", required=True)
    approve_parser.add_argument("--version", required=True)
    approve_parser.add_argument("--source-commit", required=True)
    approve_parser.add_argument("--prepare-run-id", required=True)
    approve_parser.add_argument("--approval-run-id", required=True)
    approve_parser.add_argument("--approver", required=True)
    approve_parser.add_argument("--repository", default="wtfdelphia/tg-monitor-bot")
    approve_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            prepare(args.assets, args.version, args.source_commit, args.prepare_run_id,
                    args.output, args.repository)
        else:
            if args.command == "verify":
                verify(args.assets, args.record, args.record_sha256, args.version,
                       args.source_commit, args.prepare_run_id, args.repository)
            else:
                approve(args.assets, args.record, args.record_sha256, args.version,
                        args.source_commit, args.prepare_run_id, args.approval_run_id,
                        args.approver, args.repository, args.output)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        print("stable release approval failed: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
