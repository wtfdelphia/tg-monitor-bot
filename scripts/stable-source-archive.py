#!/usr/bin/env python3
"""Create a deterministic source archive from an exact Git commit and manifest."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile


def run_git(root: Path, *arguments: str) -> bytes:
    result = subprocess.run(
        ("git", "-C", str(root), *arguments),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise ValueError(result.stderr.decode("utf-8", "replace").strip() or "git command failed")
    return result.stdout


def normalize_path(value: str) -> str:
    if not value or "\\" in value or "\x00" in value:
        raise ValueError(f"invalid manifest path: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in value.split("/")):
        raise ValueError(f"unsafe manifest path: {value!r}")
    return path.as_posix()


def load_manifest(data: bytes) -> list[str]:
    entries = []
    for line in data.decode("utf-8").splitlines():
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        entries.append(normalize_path(value))
    if not entries or len(entries) != len(set(entries)):
        raise ValueError("manifest must have unique, non-empty paths")
    return sorted(entries)


def committed_tree(root: Path, commit: str) -> dict[str, tuple[str, str, str]]:
    raw = run_git(root, "ls-tree", "-r", "-z", "--full-tree", commit)
    tree = {}
    for record in raw.split(b"\0"):
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, kind, object_id = metadata.decode("ascii").split(" ")
        path = raw_path.decode("utf-8")
        tree[path] = (mode, kind, object_id)
    return tree


def expand_manifest(entries: list[str], tree: dict[str, tuple[str, str, str]]) -> list[str]:
    for path in entries:
        if path not in tree:
            raise ValueError(f"manifest entry does not exist as an exact file in commit: {path}")
        mode, kind, _ = tree[path]
        if kind != "blob" or mode not in ("100644", "100755"):
            raise ValueError(f"unsupported tree entry {mode} {kind}: {path}")
    return sorted(entries)


def committed_content(root: Path, commit: str, paths: list[str]) -> dict[str, bytes]:
    result = {}
    for relative in paths:
        result[relative] = run_git(root, "show", f"{commit}:{relative}")
    return result


def manifest_blob(root: Path, commit: str, relative: str, tree: dict[str, tuple[str, str, str]]) -> bytes:
    entry = tree.get(relative)
    if entry is None or entry[0] not in ("100644", "100755") or entry[1] != "blob":
        raise ValueError("manifest must be a regular file in the selected commit")
    return run_git(root, "show", f"{commit}:{relative}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_archive(contents: dict[str, bytes], tree: dict[str, tuple[str, str, str]], paths: list[str], output: Path, prefix: str, epoch: int, manifest_hash: str, commit: str, manifest_path: str) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise ValueError(f"output already exists: {output}")
    temporary = output.with_name(output.name + ".tmp")
    if temporary.exists():
        raise ValueError(f"temporary output already exists: {temporary}")

    try:
        with temporary.open("xb") as raw_output:
            with raw_output:
                with gzip.GzipFile(filename="", mode="wb", fileobj=raw_output, mtime=epoch, compresslevel=9) as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                        manifest_bytes = f"manifest_sha256={manifest_hash}\nmanifest_path={manifest_path}\nsource_commit={commit}\n".encode()
                        manifest_info = tarfile.TarInfo(f"{prefix}/SOURCE-MANIFEST.sha256")
                        manifest_info.size = len(manifest_bytes)
                        manifest_info.mtime = epoch
                        manifest_info.uid = 0
                        manifest_info.gid = 0
                        manifest_info.uname = ""
                        manifest_info.gname = ""
                        manifest_info.mode = 0o644
                        archive.addfile(manifest_info, io.BytesIO(manifest_bytes))
                        for relative in paths:
                            data = contents[relative]
                            info = tarfile.TarInfo(f"{prefix}/{relative}")
                            info.size = len(data)
                            info.mtime = epoch
                            info.uid = 0
                            info.gid = 0
                            info.uname = ""
                            info.gname = ""
                            info.mode = 0o755 if tree[relative][0] == "100755" else 0o644
                            info.pax_headers = {}
                            archive.addfile(info, io.BytesIO(data))
        temporary.replace(output)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--commit", required=True, help="full 40-character Git commit SHA")
    parser.add_argument("--manifest-path", required=True, help="manifest path in source commit")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    parser.add_argument("--list", action="store_true", help="print exact archive member paths without writing an archive")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        raise SystemExit("commit must be a full lowercase SHA-1")
    root = args.repository.resolve(strict=True)
    commit = run_git(root, "rev-parse", f"{args.commit}^{{commit}}").decode().strip()
    if commit != args.commit:
        raise SystemExit("resolved commit does not match requested commit")
    tree = committed_tree(root, commit)
    manifest_relative = normalize_path(args.manifest_path)
    manifest_data = manifest_blob(root, commit, manifest_relative, tree)
    entries = load_manifest(manifest_data)
    actual_manifest_hash = hashlib.sha256(manifest_data).hexdigest()
    paths = expand_manifest(entries, tree)
    if args.manifest_path not in paths:
        raise SystemExit("manifest must list its own path so the archive is self-describing")
    prefix = f"tg-monitor-bot-source-{commit}"
    if args.list:
        print(f"{prefix}/SOURCE-MANIFEST.sha256")
        for relative in paths:
            print(f"{prefix}/{relative}")
        return 0
    if args.output is None:
        raise SystemExit("--output is required unless --list is used")
    contents = committed_content(root, commit, paths)
    write_archive(contents, tree, paths, args.output, prefix, args.source_date_epoch, actual_manifest_hash, commit, manifest_relative)
    print(f"source_commit={commit}")
    print(f"manifest_entries={len(entries)}")
    print(f"manifest_sha256={actual_manifest_hash}")
    print(f"manifest_path={manifest_relative}")
    print(f"archive_files={len(paths)}")
    print(f"archive={args.output}")
    print(f"archive_sha256={sha256_file(args.output)}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, UnicodeError, ValueError, tarfile.TarError) as error:
        raise SystemExit(f"source archive failed: {error}") from error
