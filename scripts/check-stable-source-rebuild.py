#!/usr/bin/env python3
"""Rebuild a release package using only a stable source archive and Docker."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import posixpath
import subprocess
import tarfile
import tempfile


def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, check=True, text=True, **kwargs)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-archive", type=Path, required=True)
    parser.add_argument("--sbom", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--rust-toolchain", required=True)
    parser.add_argument("--builder-image", required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    parser.add_argument("--expected-binary", type=Path)
    parser.add_argument("--expected-package", type=Path)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def verify_source_archive(archive_path: Path, source_commit: str, destination: Path) -> Path:
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        expected_prefix = f"tg-monitor-bot-source-{source_commit}/"
        names = [member.name for member in members]
        if len(names) != len(set(names)):
            raise ValueError("source archive contains duplicate paths")
        for member in members:
            if not member.name.startswith(expected_prefix):
                raise ValueError(f"unexpected archive member: {member.name}")
            relative = member.name[len(expected_prefix):]
            if relative.startswith("/") or ".." in Path(relative).parts or posixpath.normpath(relative) != relative:
                raise ValueError(f"unsafe archive member: {member.name}")
            if not member.isfile():
                raise ValueError(f"non-regular archive member: {member.name}")
        manifest_path = "scripts/stable-source-archive.manifest"
        manifest_member = archive.extractfile(expected_prefix + manifest_path)
        metadata_member = archive.extractfile(expected_prefix + "SOURCE-MANIFEST.sha256")
        if manifest_member is None or metadata_member is None:
            raise ValueError("source archive is missing manifest audit material")
        manifest_bytes = manifest_member.read()
        metadata = dict(
            line.split("=", 1)
            for line in metadata_member.read().decode("utf-8").splitlines()
            if "=" in line
        )
        if (metadata.get("source_commit") != source_commit
                or metadata.get("manifest_path") != manifest_path
                or metadata.get("manifest_sha256") != hashlib.sha256(manifest_bytes).hexdigest()):
            raise ValueError("source archive manifest metadata does not match its contents")
        listed = {
            line.strip() for line in manifest_bytes.decode("utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        expected_names = {expected_prefix + "SOURCE-MANIFEST.sha256"}
        expected_names.update(expected_prefix + path for path in listed)
        if set(names) != expected_names:
            raise ValueError("source archive members do not match the embedded manifest")
        archive.extractall(destination, filter="data")
    return destination / f"tg-monitor-bot-source-{source_commit}"


def main() -> int:
    args = parse_args()
    if not args.source_archive.is_file() or not args.sbom.is_file():
        raise SystemExit("source archive and SBOM must be regular files")
    if args.expected_package is None and args.output is None:
        raise SystemExit("provide --expected-package or --output")
    if args.expected_package is not None and args.output is not None:
        raise SystemExit("--expected-package and --output are mutually exclusive")
    if args.expected_package is not None and args.expected_binary is None:
        raise SystemExit("--expected-binary is required when comparing a release package")

    with tempfile.TemporaryDirectory(prefix="tgm-source-rebuild-") as temporary:
        root = Path(temporary)
        source = root / "source"
        source.mkdir()
        extracted = verify_source_archive(args.source_archive, args.source_commit, source)
        dockerfile = extracted / "deploy/native/centos7-builder.Dockerfile"
        if not dockerfile.is_file():
            raise SystemExit("source archive is missing the CentOS 7 builder")

        image = f"tgm-source-rebuild:{os.getpid()}"
        container = ""
        try:
            run(["docker", "build", "--pull=false", "--no-cache", "-f", str(dockerfile), "-t", image, str(extracted)])
            container = subprocess.check_output(("docker", "create", image), text=True).strip()
            binary = root / "tgm"
            run(["docker", "cp", f"{container}:/src/target/release/tgm", str(binary)])
            if args.expected_binary is not None and sha256(binary) != sha256(args.expected_binary.resolve(strict=True)):
                raise SystemExit("source archive rebuilt binary differs from release binary")
            output = args.output or (root / "package")
            run([
                os.environ.get("PYTHON", "python3"),
                str(extracted / "scripts/native-release-package.py"),
                "--source-root", str(extracted),
                "--binary", str(binary),
                "--sbom", str(args.sbom.resolve(strict=True)),
                "--version", args.version,
                "--source-commit", args.source_commit,
                "--rust-toolchain", args.rust_toolchain,
                "--builder-image", args.builder_image,
                "--source-manifest-sha256", hashlib.sha256(
                    (extracted / "scripts/stable-source-archive.manifest").read_bytes()
                ).hexdigest(),
                "--source-date-epoch", str(args.source_date_epoch),
                "--output", str(output.resolve()),
            ])
            archive_name = f"tgm-{args.version}-linux-x86_64-gnu.tar.gz"
            rebuilt = output / archive_name
            if args.expected_package is not None:
                expected = args.expected_package.resolve(strict=True)
                if sha256(rebuilt) != sha256(expected):
                    raise SystemExit("source archive regenerated package differs from release package")
                print(f"source_rebuild=pass package_sha256={sha256(rebuilt)}")
            else:
                print(f"source_rebuild=pass package={rebuilt} package_sha256={sha256(rebuilt)}")
        finally:
            if container:
                subprocess.run(("docker", "rm", "-f", container), check=False, stdout=subprocess.DEVNULL)
            subprocess.run(("docker", "image", "rm", image), check=False, stdout=subprocess.DEVNULL)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
