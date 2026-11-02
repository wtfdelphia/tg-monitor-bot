#!/usr/bin/env python3
"""Build the deterministic contents of the native release archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="构建 TGM 原生二进制发布包")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sbom", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--rust-toolchain", required=True)
    parser.add_argument("--builder-image", required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def set_tree_mtime(root: Path, timestamp: int) -> None:
    paths = sorted(root.rglob("*"), key=lambda path: len(path.parts), reverse=True)
    for path in paths + [root]:
        os.utime(path, (timestamp, timestamp), follow_symlinks=False)


def write_checksums(root: Path) -> None:
    files = sorted(
        path.relative_to(root).as_posix()
        for path in root.rglob("*")
        if path.is_file() and path.name != "SHA256SUMS"
    )
    lines = [f"{sha256(root / relative)}  {relative}" for relative in files]
    (root / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    repository = Path(__file__).resolve().parents[1]
    binary = args.binary.resolve()
    sbom = args.sbom.resolve()
    if not binary.is_file():
        raise SystemExit(f"二进制不存在：{binary}")
    if not sbom.is_file():
        raise SystemExit(f"SBOM 不存在：{sbom}")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        raise SystemExit("版本号必须是 MAJOR.MINOR.PATCH")
    if args.output.exists() and any(args.output.iterdir()):
        raise SystemExit(f"输出目录非空：{args.output}")
    args.output.mkdir(parents=True, exist_ok=True)

    archive_name = f"tgm-{args.version}-linux-x86_64-gnu.tar.gz"
    with tempfile.TemporaryDirectory(prefix="tgm-native-package-") as temporary:
        staging = Path(temporary)
        (staging / "bin").mkdir()
        (staging / "deploy" / "systemd").mkdir(parents=True)
        (staging / "scripts").mkdir()
        shutil.copy2(binary, staging / "bin" / "tgm")
        (staging / "bin" / "tgm").chmod(0o755)
        shutil.copy2(sbom, staging / "sbom.cdx.json")
        (staging / "VERSION").write_text(f"{args.version}\n", encoding="utf-8")
        shutil.copy2(repository / "scripts" / "install-native.sh", staging / "scripts")
        (staging / "scripts" / "install-native.sh").chmod(0o755)
        shutil.copy2(repository / "scripts" / "native-release-preflight.sh", staging / "scripts")
        (staging / "scripts" / "native-release-preflight.sh").chmod(0o755)
        shutil.copy2(repository / "deploy" / "native" / "README.md", staging / "README.md")
        readme = (staging / "README.md").read_text(encoding="utf-8")
        readme = readme.replace("@VERSION@", args.version)
        readme = readme.replace("@SOURCE_COMMIT@", args.source_commit)
        readme = readme.replace("@TARGET@", args.target)
        (staging / "README.md").write_text(readme, encoding="utf-8")
        for source in sorted((repository / "deploy" / "systemd").iterdir()):
            if source.is_file():
                shutil.copy2(source, staging / "deploy" / "systemd" / source.name)

        buildinfo = {
            "artifact": archive_name,
            "version": args.version,
            "source_commit": args.source_commit,
            "target": args.target,
            "rust_toolchain": args.rust_toolchain,
            "builder_image": args.builder_image,
            "source_date_epoch": args.source_date_epoch,
            "cargo_lock_sha256": sha256(repository / "Cargo.lock"),
            "openssl": "vendored via josekit feature",
        }
        (staging / "BUILDINFO.json").write_text(
            json.dumps(buildinfo, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        write_checksums(staging)
        set_tree_mtime(staging, args.source_date_epoch)

        package_root = args.output / f"tgm-{args.version}-linux-x86_64-gnu"
        package_root.mkdir()
        for child in sorted(staging.iterdir()):
            destination = package_root / child.name
            if child.is_dir():
                shutil.copytree(child, destination)
            else:
                shutil.copy2(child, destination)
        set_tree_mtime(package_root, args.source_date_epoch)
        archive_tar = args.output / archive_name.removesuffix(".gz")
        archive = args.output / archive_name
        subprocess.run(
            [
                "tar",
                "--sort=name",
                f"--mtime=@{args.source_date_epoch}",
                "--owner=0",
                "--group=0",
                "--numeric-owner",
                "-cf",
                str(archive_tar),
                "-C",
                str(package_root),
                ".",
            ],
            check=True,
        )
        subprocess.run(["gzip", "-n", str(archive_tar)], check=True)
        shutil.copy2(staging / "BUILDINFO.json", args.output / "BUILDINFO.json")
        shutil.copy2(staging / "sbom.cdx.json", args.output / "sbom.cdx.json")
        shutil.rmtree(package_root)

    print(f"发布包：{args.output / archive_name}")
    print(f"发布包 SHA-256：{sha256(args.output / archive_name)}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        print(f"打包命令失败：{error}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
