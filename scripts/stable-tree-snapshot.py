#!/usr/bin/env python3
"""Write a Git tree containing exactly the paths in a committed stable manifest."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def git(root: Path, *args: str, env: dict[str, str] | None = None, input_data: bytes | None = None) -> bytes:
    result = subprocess.run(
        ("git", "-C", str(root), *args),
        input=input_data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        check=False,
    )
    if result.returncode:
        raise ValueError(result.stderr.decode("utf-8", "replace").strip() or "git command failed")
    return result.stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--commit", required=True)
    parser.add_argument("--manifest-path", required=True)
    args = parser.parse_args()
    root = args.repository.resolve(strict=True)
    commit = git(root, "rev-parse", f"{args.commit}^{{commit}}").decode().strip()
    records = git(root, "ls-tree", "-r", "-z", "--full-tree", commit)
    entries: dict[str, tuple[str, str]] = {}
    for record in records.split(b"\0"):
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, kind, oid = metadata.decode("ascii").split(" ")
        entries[raw_path.decode("utf-8")] = (mode, oid)
    manifest_entry = entries.get(args.manifest_path)
    if manifest_entry is None or manifest_entry[0] not in ("100644", "100755"):
        raise SystemExit("stable tree manifest is missing or not a regular file")
    manifest = git(root, "show", f"{commit}:{args.manifest_path}").decode("utf-8")
    paths = [
        line.strip() for line in manifest.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not paths or len(paths) != len(set(paths)):
        raise SystemExit("stable tree manifest must have unique paths")
    if args.manifest_path not in paths:
        raise SystemExit("stable tree manifest must include itself")
    index_root = Path(tempfile.mkdtemp(prefix="stable-tree-index-"))
    index_path = index_root / "index"
    environment = os.environ.copy()
    environment["GIT_INDEX_FILE"] = str(index_path)
    try:
        git(root, "read-tree", "--empty", env=environment)
        for path in paths:
            item = entries.get(path)
            if item is None:
                raise ValueError(f"stable tree path is missing from commit: {path}")
            mode, oid = item
            if mode not in ("100644", "100755", "120000"):
                raise ValueError(f"stable tree path is not a regular file: {path}")
            if mode == "120000":
                target = git(root, "show", f"{commit}:{path}").decode("utf-8", "strict")
                if target.startswith("/") or "\\" in target or "\x00" in target:
                    raise ValueError(f"stable tree symlink target is unsafe: {path}")
                target_path = Path(path).parent / target
                if ".." in target_path.parts and target_path.resolve().is_absolute():
                    normalized = (Path(path).parent / target).as_posix()
                    parts = []
                    for part in normalized.split("/"):
                        if part in ("", "."):
                            continue
                        if part == "..":
                            if not parts:
                                raise ValueError(f"stable tree symlink escapes repository: {path}")
                            parts.pop()
                        else:
                            parts.append(part)
                    target_name = "/".join(parts)
                else:
                    target_name = target_path.as_posix()
                if target_name not in paths:
                    raise ValueError(f"stable tree symlink target is not allowlisted: {path} -> {target_name}")
            git(root, "update-index", "--add", "--cacheinfo", f"{mode},{oid},{path}", env=environment)
        tree = git(root, "write-tree", env=environment).decode().strip()
        print(f"stable_tree_snapshot=pass commit={commit} tree={tree} files={len(paths)}")
        print(f"{tree}")
    finally:
        index_path.unlink(missing_ok=True)
        index_root.rmdir()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, UnicodeError, ValueError) as error:
        raise SystemExit(f"stable tree snapshot failed: {error}") from error
