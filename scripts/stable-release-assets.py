#!/usr/bin/env python3
"""Prepare and verify the exact signed assets for a stable GitHub release."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import posixpath
import subprocess
import sys
import tarfile


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


def verify_source_archive(path: Path, version: str, commit: str, manifest: bytes) -> None:
    prefix = f"tg-monitor-bot-source-{commit}/"
    expected = {prefix + "SOURCE-MANIFEST.sha256"}
    for line in manifest.decode("utf-8").splitlines():
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        if (value.startswith("/") or ".." in value.split("/") or "\\" in value
                or posixpath.normpath(value) != value):
            fail("source manifest contains an unsafe path")
        if prefix + value in expected:
            fail("source manifest contains duplicate paths")
        expected.add(prefix + value)
    try:
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)) or set(names) != expected:
                fail("source archive members do not exactly match its committed manifest")
            if any(not member.isfile() for member in members):
                fail("source archive contains a non-regular file")
            metadata_member = archive.extractfile(prefix + "SOURCE-MANIFEST.sha256")
            if metadata_member is None:
                fail("source archive is missing manifest metadata")
            metadata = dict(
                line.split("=", 1)
                for line in metadata_member.read().decode("utf-8").splitlines()
                if "=" in line
            )
            if (metadata.get("source_commit") != commit
                    or metadata.get("manifest_path") != "scripts/stable-source-archive.manifest"
                    or metadata.get("manifest_sha256") != hashlib.sha256(manifest).hexdigest()):
                fail("source archive metadata does not match release provenance")
            embedded_manifest = archive.extractfile(prefix + "scripts/stable-source-archive.manifest")
            if embedded_manifest is None or embedded_manifest.read() != manifest:
                fail("source archive does not contain the signed source manifest")
    except (OSError, tarfile.TarError, UnicodeError) as error:
        fail(f"cannot validate source archive: {error}")


def verify_binary_package(path: Path, version: str, buildinfo: Path, sbom: Path) -> None:
    try:
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            names = []
            file_names = set()
            for member in members:
                name = member.name[2:] if member.name.startswith("./") else member.name
                if member.isdir():
                    name = name.rstrip("/")
                    if name in ("", "."):
                        continue
                else:
                    file_names.add(name)
                names.append(name)
            if (len(names) != len(set(names))
                    or any(not (member.isfile() or member.isdir()) for member in members)
                    or any(name.startswith("/") or "\\" in name or ".." in name.split("/")
                           or posixpath.normpath(name) != name for name in names)):
                fail("binary archive contains duplicate or non-regular members")
            for relative, expected in (("BUILDINFO.json", buildinfo), ("sbom.cdx.json", sbom)):
                member = archive.extractfile("./" + relative)
                if member is None or member.read() != expected.read_bytes():
                    fail(f"binary package {relative} differs from the signed Release asset")
            manifest_member = archive.extractfile("./SHA256SUMS")
            if manifest_member is None:
                fail("binary archive is missing its internal SHA256SUMS")
            checksums = {}
            for line in manifest_member.read().decode("ascii").splitlines():
                match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._/@-]+)", line)
                if match is None or match.group(2) in checksums:
                    fail("binary archive SHA256SUMS is invalid or contains duplicates")
                checksums[match.group(2)] = match.group(1)
            if not checksums or set(checksums) | {"SHA256SUMS"} != file_names:
                fail("binary archive members do not match internal SHA256SUMS")
            for relative, digest in checksums.items():
                member = archive.extractfile("./" + relative)
                if member is None or hashlib.sha256(member.read()).hexdigest() != digest:
                    fail(f"binary archive internal checksum mismatch: {relative}")
    except (OSError, tarfile.TarError, UnicodeError) as error:
        fail(f"cannot validate binary archive: {error}")


def prepare(args: argparse.Namespace) -> None:
    assets = args.assets.resolve(strict=True)
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        fail("version must be MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        fail("source commit must be a full lowercase SHA-1")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.builder_image):
        fail("builder image must be an image ID with sha256 digest")
    if not re.fullmatch(r"[1-9][0-9]*", args.run_id):
        fail("run ID must be a positive integer")
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
            or buildinfo.get("target") != "x86_64-unknown-linux-gnu"
            or buildinfo.get("channel") != "stable"
            or buildinfo.get("stable") is not True
            or buildinfo.get("release_tag") != f"v{args.version}"):
        fail("BUILDINFO does not match release inputs")
    source_manifest_digest = args.source_manifest_sha256
    if not re.fullmatch(r"[0-9a-f]{64}", source_manifest_digest):
        fail("source manifest SHA-256 must be supplied from the committed source tree")
    manifest_digest = sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")
    if manifest_digest != source_manifest_digest:
        fail("release manifest differs from the committed source manifest")
    if buildinfo.get("source_manifest_sha256") != manifest_digest:
        fail("BUILDINFO source manifest digest differs from the committed source manifest")
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
        "SHA256SUMS", "provenance.json",
    }
    actual_names = {path.name for path in assets.iterdir()}
    if any(not (assets / name).is_file() or (assets / name).is_symlink() for name in actual_names):
        fail("stable release assets must be regular files")
    signature_names = {"SHA256SUMS.sigstore.json", "provenance.sigstore.json"}
    if not signature_names.issubset(actual_names) and signature_names.intersection(actual_names):
        fail("stable release signature bundles must be both present or both absent")
    if signature_names.issubset(actual_names):
        expected_names.update(signature_names)
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
            or buildinfo.get("target") != "x86_64-unknown-linux-gnu"
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", str(buildinfo.get("builder_image", "")))
            or not isinstance(buildinfo.get("rust_toolchain"), str)
            or not buildinfo["rust_toolchain"]
            or buildinfo.get("channel") != "stable"
            or buildinfo.get("stable") is not True
            or buildinfo.get("release_tag") != f"v{args.version}"
            or buildinfo.get("artifact") != f"tgm-{args.version}-linux-x86_64-gnu.tar.gz"
            or buildinfo.get("source_manifest_sha256") != sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")):
        fail("BUILDINFO does not match release version, commit or target")
    manifest_bytes = (assets / "SOURCE-ARCHIVE-MANIFEST.txt").read_bytes()
    verify_source_archive(assets / f"tgm-{args.version}-source.tar.gz", args.version,
                          args.source_commit, manifest_bytes)
    verify_binary_package(assets / f"tgm-{args.version}-linux-x86_64-gnu.tar.gz",
                          args.version, assets / "BUILDINFO.json", assets / "sbom.cdx.json")
    provenance = json.loads((assets / "provenance.json").read_text(encoding="utf-8"))
    expected_subjects = [
        {"name": name, "digest": {"sha256": digest}}
        for name, digest in sorted(checksums.items())
    ]
    if provenance.get("_type") != "https://in-toto.io/Statement/v1" or provenance.get("predicateType") != "https://slsa.dev/provenance/v1":
        fail("provenance statement type is invalid")
    if provenance.get("subject") != expected_subjects:
        fail("provenance subjects do not exactly match SHA256SUMS")
    full_predicate = provenance.get("predicate", {})
    predicate = full_predicate.get("buildDefinition", {})
    if predicate.get("buildType") != (
            f"https://github.com/{args.repository}/.github/workflows/binary-release.yml"):
        fail("provenance stable workflow identity mismatch")
    expected_dependency = {
        "uri": f"git+https://github.com/{args.repository}@refs/tags/v{args.version}",
        "digest": {"gitCommit": args.source_commit},
    }
    if predicate.get("resolvedDependencies") != [expected_dependency]:
        fail("provenance source commit or release tag mismatch")
    parameters = predicate.get("externalParameters", {})
    if (parameters.get("release_tag") != f"v{args.version}"
            or parameters.get("source_manifest") != "scripts/stable-source-archive.manifest"
            or parameters.get("source_manifest_sha256") != sha256(assets / "SOURCE-ARCHIVE-MANIFEST.txt")
            or parameters.get("builder_image") != buildinfo.get("builder_image")
            or parameters.get("rust_toolchain") != buildinfo.get("rust_toolchain")
            or buildinfo.get("source_manifest_sha256") != parameters.get("source_manifest_sha256")):
        fail("provenance release, manifest, builder or toolchain mismatch")
    run_details = provenance.get("predicate", {}).get("runDetails", {})
    run_id = run_details.get("builder", {}).get("id", "").rsplit("/", 1)[-1]
    if (not run_id.isdigit()
            or run_details.get("builder", {}).get("id") != f"https://github.com/{args.repository}/actions/runs/{run_id}"
            or run_details.get("metadata", {}).get("invocationId") != run_id):
        fail("provenance run details are invalid")
    expected_run_id = getattr(args, "expected_run_id", None)
    if expected_run_id is not None and run_id != expected_run_id:
        fail("provenance run ID does not match the signing workflow run")
    if getattr(args, "verify_signatures", False):
        if not signature_names.issubset(actual_names):
            fail("signature bundles are required when signature verification is requested")
        identity = (
            r"https://github\.com/wtfdelphia/tg-monitor-bot/.github/workflows/"
            r"binary-release\.yml@refs/tags/v" + re.escape(args.version)
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
    parser.add_argument("--expected-run-id")
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
