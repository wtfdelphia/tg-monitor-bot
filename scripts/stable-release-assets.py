#!/usr/bin/env python3
"""Prepare and verify the exact signed assets for a stable GitHub release."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def fail(message: str) -> None:
    raise ValueError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def expected_payloads(assets: Path, version: str) -> dict[str, Path]:
    return {
        f"tgm-{version}-linux-x86_64-gnu.tar.gz": assets / f"tgm-{version}-linux-x86_64-gnu.tar.gz",
        f"tgm-{version}-source.tar.gz": assets / f"tgm-{version}-source.tar.gz",
        "SOURCE-ARCHIVE-MANIFEST.txt": assets / "SOURCE-ARCHIVE-MANIFEST.txt",
        "BUILDINFO.json": assets / "BUILDINFO.json",
        "sbom.cdx.json": assets / "sbom.cdx.json",
    }


def read_checksums(path: Path) -> dict[str, str]:
    entries: dict[str, str] = {}
    for line in path.read_text(encoding="ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if match is None or match.group(2) in entries:
            fail("SHA256SUMS contains an invalid or duplicate entry")
        entries[match.group(2)] = match.group(1)
    return entries


def prepare(args: argparse.Namespace) -> None:
    assets = args.assets.resolve(strict=True)
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        fail("version must be MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        fail("source commit must be a full lowercase SHA-1")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.builder_image):
        fail("builder image must be an image ID with sha256 digest")
    payloads = expected_payloads(assets, args.version)
    actual = {path.name for path in assets.iterdir()}
    if actual != set(payloads):
        fail(f"release payload set mismatch: actual={sorted(actual)} expected={sorted(payloads)}")
    for name, path in payloads.items():
        if not path.is_file() or path.is_symlink():
            fail(f"release payload is not a regular file: {name}")
    buildinfo = json.loads((assets / "BUILDINFO.json").read_text(encoding="utf-8"))
    if (buildinfo.get("version") != args.version
            or buildinfo.get("source_commit") != args.source_commit
            or buildinfo.get("builder_image") != args.builder_image
            or buildinfo.get("rust_toolchain") != args.rust_toolchain
            or buildinfo.get("target") != "x86_64-unknown-linux-gnu"):
        fail("BUILDINFO does not match release inputs")
    source_manifest_digest = args.source_manifest_sha256
    if not re.fullmatch(r"[0-9a-f]{64}", source_manifest_digest):
        fail("source manifest SHA-256 must be supplied from the committed source tree")
    manifest_digest = sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")
    if manifest_digest != source_manifest_digest:
        fail("release manifest differs from the committed source manifest")
    lines = [f"{sha256(path)}  {name}" for name, path in sorted(payloads.items())]
    (assets / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="ascii")
    provenance = {
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [
            {"name": name, "digest": {"sha256": sha256(path)}}
            for name, path in sorted(payloads.items())
        ],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": f"https://github.com/{args.repository}/.github/workflows/binary-release.yml",
                "externalParameters": {
                    "release_tag": f"v{args.version}",
                    "source_manifest": "scripts/stable-source-archive.manifest",
                    "source_manifest_sha256": manifest_digest,
                    "builder_image": args.builder_image,
                    "rust_toolchain": args.rust_toolchain,
                },
                "resolvedDependencies": [{
                    "uri": f"git+https://github.com/{args.repository}@refs/tags/v{args.version}",
                    "digest": {"gitCommit": args.source_commit},
                }],
            },
            "runDetails": {
                "builder": {"id": f"https://github.com/{args.repository}/actions/runs/{args.run_id}"},
                "metadata": {"invocationId": args.run_id},
            },
        },
    }
    (assets / "provenance.json").write_text(json.dumps(provenance, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"stable_release_assets=prepared files={len(payloads)} manifest_sha256={manifest_digest}")


def verify(args: argparse.Namespace) -> None:
    assets = args.assets.resolve(strict=True)
    expected_names = {
        *expected_payloads(assets, args.version).keys(),
        "SHA256SUMS", "SHA256SUMS.sigstore.json", "provenance.json", "provenance.sigstore.json",
    }
    actual_names = {path.name for path in assets.iterdir()}
    if actual_names != expected_names:
        fail(f"stable release asset set mismatch: actual={sorted(actual_names)} expected={sorted(expected_names)}")
    checksums = read_checksums(assets / "SHA256SUMS")
    expected_payload_names = set(expected_payloads(assets, args.version))
    if set(checksums) != expected_payload_names:
        fail("SHA256SUMS does not bind exactly the stable release payloads")
    for name, digest in checksums.items():
        if sha256(assets / name) != digest:
            fail(f"SHA256 mismatch: {name}")
    buildinfo = json.loads((assets / "BUILDINFO.json").read_text(encoding="utf-8"))
    if (buildinfo.get("version") != args.version
            or buildinfo.get("source_commit") != args.source_commit
            or buildinfo.get("target") != "x86_64-unknown-linux-gnu"):
        fail("BUILDINFO does not match release version, commit or target")
    provenance = json.loads((assets / "provenance.json").read_text(encoding="utf-8"))
    expected_subjects = [
        {"name": name, "digest": {"sha256": digest}}
        for name, digest in sorted(checksums.items())
    ]
    if provenance.get("_type") != "https://in-toto.io/Statement/v1" or provenance.get("predicateType") != "https://slsa.dev/provenance/v1":
        fail("provenance statement type is invalid")
    if provenance.get("subject") != expected_subjects:
        fail("provenance subjects do not exactly match SHA256SUMS")
    predicate = provenance.get("predicate", {}).get("buildDefinition", {})
    expected_dependency = {
        "uri": f"git+https://github.com/{args.repository}@refs/tags/v{args.version}",
        "digest": {"gitCommit": args.source_commit},
    }
    if predicate.get("resolvedDependencies") != [expected_dependency]:
        fail("provenance source commit or release tag mismatch")
    parameters = predicate.get("externalParameters", {})
    if (parameters.get("release_tag") != f"v{args.version}"
            or parameters.get("source_manifest_sha256") != sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")
            or parameters.get("builder_image") != buildinfo.get("builder_image")
            or parameters.get("rust_toolchain") != buildinfo.get("rust_toolchain")):
        fail("provenance release, manifest, builder or toolchain mismatch")
    run_details = provenance.get("predicate", {}).get("runDetails", {})
    run_id = run_details.get("builder", {}).get("id", "").rsplit("/", 1)[-1]
    if (not run_id.isdigit()
            or run_details.get("builder", {}).get("id") != f"https://github.com/{args.repository}/actions/runs/{run_id}"
            or run_details.get("metadata", {}).get("invocationId") != run_id):
        fail("provenance run details are invalid")
    if getattr(args, "verify_signatures", False):
        identity = (
            r"https://github\.com/wtfdelphia/tg-monitor-bot/.github/workflows/"
            r"binary-release\.yml@refs/(tags/v[0-9]+\.[0-9]+\.[0-9]+|heads/main)"
        )
        issuer = "https://token.actions.githubusercontent.com"
        for blob, bundle in (("SHA256SUMS", "SHA256SUMS.sigstore.json"),
                             ("provenance.json", "provenance.sigstore.json")):
            subprocess.run([
                "cosign", "verify-blob", "--bundle", str(assets / bundle),
                "--certificate-identity-regexp", identity,
                "--certificate-oidc-issuer", issuer, str(assets / blob),
            ], check=True, stdout=subprocess.DEVNULL)
    print(f"stable_release_assets=pass files={len(actual_names)} payloads={len(checksums)}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "verify"))
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--repository", default="wtfdelphia/tg-monitor-bot")
    parser.add_argument("--builder-image")
    parser.add_argument("--rust-toolchain")
    parser.add_argument("--run-id")
    parser.add_argument("--source-manifest-sha256")
    parser.add_argument("--verify-signatures", action="store_true")
    args = parser.parse_args()
    try:
        if args.mode == "prepare":
            if not all((args.builder_image, args.rust_toolchain, args.run_id)):
                fail("prepare requires builder image, Rust toolchain and run ID")
            prepare(args)
        else:
            verify(args)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        print(f"stable release assets failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
