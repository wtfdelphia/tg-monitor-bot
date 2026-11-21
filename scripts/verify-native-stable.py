#!/usr/bin/env python3
"""Verify signed stable release assets before installing a native package."""

from __future__ import print_function

import argparse
import hashlib
import json
import posixpath
import re
import subprocess
import sys
import tarfile


REPOSITORY = "wtfdelphia/tg-monitor-bot"
ISSUER = "https://token.actions.githubusercontent.com"


def fail(message):
    raise ValueError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def digest_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)


def read_json(path):
    with open(path, "r") as stream:
        value = json.load(stream)
    if not isinstance(value, dict):
        fail("JSON root must be an object: " + path)
    return value


def verify_signature(path, bundle, version):
    identity = (
        r"https://github\.com/wtfdelphia/tg-monitor-bot/.github/workflows/"
        r"binary-release\.yml@refs/tags/" + re.escape("v" + version)
    )
    subprocess.check_call([
        "cosign", "verify-blob", "--offline", "--bundle", bundle,
        "--certificate-identity-regexp", identity,
        "--certificate-oidc-issuer", ISSUER, path,
    ], stdout=subprocess.DEVNULL)


def read_checksums(path):
    entries = {}
    with open(path, "r") as stream:
        for line in stream.read().splitlines():
            match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
            if match is None or match.group(2) in entries:
                fail("SHA256SUMS is invalid or contains duplicate names")
            entries[match.group(2)] = match.group(1)
    return entries


def safe_regular_members(archive, allow_directories):
    members = archive.getmembers()
    names = []
    seen = set()
    for member in members:
        name = member.name[2:] if member.name.startswith("./") else member.name
        if (not member.isfile() and not (allow_directories and member.isdir())):
            fail("archive contains a link or special member")
        if member.isdir():
            name = name.rstrip("/")
            if name in ("", ".") and allow_directories:
                continue
        if (name.startswith("/") or "\\" in name or ".." in name.split("/")
                or posixpath.normpath(name) != name):
            fail("archive contains an unsafe path")
        if name in seen:
            fail("archive contains duplicate paths")
        seen.add(name)
        if member.isfile():
            names.append(name)
    return members, names


def verify_binary_archive(path, version, buildinfo_bytes, sbom_bytes):
    with tarfile.open(path, "r:gz") as archive:
        members, names = safe_regular_members(archive, True)
        files = {}
        for relative in ("BUILDINFO.json", "sbom.cdx.json", "VERSION", "SHA256SUMS"):
            member = archive.extractfile("./" + relative)
            if member is None:
                fail("binary archive is missing " + relative)
            files[relative] = member.read()
        if files["BUILDINFO.json"] != buildinfo_bytes or files["sbom.cdx.json"] != sbom_bytes:
            fail("embedded BUILDINFO or SBOM differs from signed release assets")
        if files["VERSION"].decode("ascii").strip() != version:
            fail("binary archive VERSION mismatch")
        checksums = {}
        for line in files["SHA256SUMS"].decode("ascii").splitlines():
            match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._/@-]+)", line)
            if match is None or match.group(2) in checksums:
                fail("binary archive SHA256SUMS is invalid")
            relative = match.group(2)
            if relative.startswith("/") or ".." in relative.split("/") or posixpath.normpath(relative) != relative:
                fail("binary archive checksum path is unsafe")
            checksums[relative] = match.group(1)
        if not checksums or set(checksums) | {"SHA256SUMS"} != set(names):
            fail("binary archive members do not match internal SHA256SUMS")
        by_name = {
            member.name[2:] if member.name.startswith("./") else member.name: member
            for member in members if member.isfile()
        }
        for name, expected in checksums.items():
            member = by_name.get(name)
            if member is None:
                fail("binary archive checksum member is missing: " + name)
            stream = archive.extractfile(member)
            digest = hashlib.sha256()
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
            if digest.hexdigest() != expected:
                fail("binary archive internal checksum mismatch: " + name)


def verify_source_archive(path, commit, manifest):
    prefix = "tg-monitor-bot-source-" + commit + "/"
    expected = set([prefix + "SOURCE-MANIFEST.sha256"])
    for line in manifest.decode("utf-8").splitlines():
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        if (value.startswith("/") or "\\" in value or ".." in value.split("/")
                or posixpath.normpath(value) != value or prefix + value in expected):
            fail("source manifest contains an unsafe or duplicate path")
        expected.add(prefix + value)
    with tarfile.open(path, "r:gz") as archive:
        members, _ = safe_regular_members(archive, False)
        names = [member.name for member in members]
        if set(names) != expected:
            fail("source archive members do not exactly match the committed manifest")
        embedded = archive.extractfile(prefix + "scripts/stable-source-archive.manifest")
        metadata_member = archive.extractfile(prefix + "SOURCE-MANIFEST.sha256")
        if embedded is None or embedded.read() != manifest or metadata_member is None:
            fail("source archive does not match the signed manifest")
        metadata = {}
        for line in metadata_member.read().decode("utf-8").splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                metadata[key] = value
        if (metadata.get("source_commit") != commit
                or metadata.get("manifest_path") != "scripts/stable-source-archive.manifest"
                or metadata.get("manifest_sha256") != sha256(manifest)):
            fail("source archive metadata does not match source identity")


def verify(args):
    archive_name = "tgm-" + args.version + "-linux-x86_64-gnu.tar.gz"
    source_name = "tgm-" + args.version + "-source.tar.gz"
    manifest_name = "SOURCE-ARCHIVE-MANIFEST.txt"
    buildinfo = read_json(args.buildinfo)
    sbom = read_json(args.sbom)
    if (buildinfo.get("channel") != "stable" or buildinfo.get("stable") is not True
            or buildinfo.get("version") != args.version
            or buildinfo.get("release_tag") != "v" + args.version
            or buildinfo.get("artifact") != archive_name
            or buildinfo.get("source_commit") != args.source_commit
            or buildinfo.get("target") != "x86_64-unknown-linux-gnu"
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", str(buildinfo.get("builder_image", "")))
            or not isinstance(buildinfo.get("rust_toolchain"), str)
            or not buildinfo["rust_toolchain"]):
        fail("BUILDINFO does not match stable version and source")
    if sbom.get("bomFormat") != "CycloneDX":
        fail("SBOM is not CycloneDX")
    with open(args.source_manifest, "rb") as stream:
        manifest = stream.read()
    if sha256(manifest) != buildinfo.get("source_manifest_sha256"):
        fail("source manifest digest differs from BUILDINFO")

    expected = {
        archive_name: digest_file(args.archive),
        source_name: digest_file(args.source_archive),
        manifest_name: digest_file(args.source_manifest),
        "BUILDINFO.json": digest_file(args.buildinfo),
        "sbom.cdx.json": digest_file(args.sbom),
    }
    if read_checksums(args.checksums) != expected:
        fail("SHA256SUMS must bind exactly all stable payloads")
    verify_signature(args.checksums, args.checksum_bundle, args.version)
    verify_signature(args.provenance, args.provenance_bundle, args.version)
    provenance = read_json(args.provenance)
    subjects = [{"name": name, "digest": {"sha256": digest}}
                for name, digest in sorted(expected.items())]
    if (provenance.get("_type") != "https://in-toto.io/Statement/v1"
            or provenance.get("predicateType") != "https://slsa.dev/provenance/v1"
            or provenance.get("subject") != subjects):
        fail("provenance subjects do not exactly match stable SHA256SUMS")
    predicate = provenance.get("predicate", {})
    definition = predicate.get("buildDefinition", {})
    if definition.get("buildType") != "https://github.com/" + REPOSITORY + "/.github/workflows/binary-release.yml":
        fail("provenance workflow identity mismatch")
    dependency = {
        "uri": "git+https://github.com/" + REPOSITORY + "@refs/tags/v" + args.version,
        "digest": {"gitCommit": args.source_commit},
    }
    if definition.get("resolvedDependencies") != [dependency]:
        fail("provenance source commit or tag mismatch")
    parameters = definition.get("externalParameters", {})
    if (parameters.get("release_tag") != "v" + args.version
            or parameters.get("source_manifest") != "scripts/stable-source-archive.manifest"
            or parameters.get("source_manifest_sha256") != sha256(manifest)
            or parameters.get("builder_image") != buildinfo.get("builder_image")
            or parameters.get("rust_toolchain") != buildinfo.get("rust_toolchain")):
        fail("provenance release parameters mismatch")
    run_details = predicate.get("runDetails", {})
    builder = run_details.get("builder", {}).get("id", "")
    run_id = builder.rsplit("/", 1)[-1]
    if (not run_id.isdigit()
            or builder != "https://github.com/" + REPOSITORY + "/actions/runs/" + run_id
            or run_details.get("metadata", {}).get("invocationId") != run_id):
        fail("provenance run details invalid")
    with open(args.buildinfo, "rb") as stream:
        buildinfo_bytes = stream.read()
    with open(args.sbom, "rb") as stream:
        sbom_bytes = stream.read()
    verify_binary_archive(args.archive, args.version, buildinfo_bytes, sbom_bytes)
    verify_source_archive(args.source_archive, args.source_commit, manifest)
    print("稳定发布来源验证通过：v" + args.version + " commit=" + args.source_commit)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True)
    parser.add_argument("--checksums", required=True)
    parser.add_argument("--checksum-bundle", required=True)
    parser.add_argument("--provenance", required=True)
    parser.add_argument("--provenance-bundle", required=True)
    parser.add_argument("--buildinfo", required=True)
    parser.add_argument("--sbom", required=True)
    parser.add_argument("--source-archive", required=True)
    parser.add_argument("--source-manifest", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        fail("version must be MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
        fail("source commit must be a full lowercase SHA-1")
    verify(args)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (IOError, OSError, UnicodeError, ValueError, json.JSONDecodeError,
            subprocess.CalledProcessError, tarfile.TarError) as error:
        print("stable release verification failed: " + str(error), file=sys.stderr)
        sys.exit(1)
