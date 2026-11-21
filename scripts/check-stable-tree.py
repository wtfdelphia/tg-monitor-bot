#!/usr/bin/env python3
"""Check that a committed stable tree exactly matches its reviewed path manifest."""

from __future__ import annotations

import argparse
import re
from pathlib import Path, PurePosixPath
from urllib.parse import unquote
import subprocess
import sys


def git(root: Path, *arguments: str) -> bytes:
    result = subprocess.run(
        ("git", "-C", str(root), *arguments),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise ValueError(result.stderr.decode("utf-8", "replace").strip() or "git command failed")
    return result.stdout


def normalize(value: str) -> str:
    path = PurePosixPath(value)
    if not value or "\\" in value or "\x00" in value or path.is_absolute():
        raise ValueError(f"invalid stable tree path: {value!r}")
    if any(part in ("", ".", "..") for part in value.split("/")):
        raise ValueError(f"unsafe stable tree path: {value!r}")
    return path.as_posix()


def manifest_paths(data: bytes) -> set[str]:
    entries = []
    for raw_line in data.decode("utf-8").splitlines():
        value = raw_line.strip()
        if value and not value.startswith("#"):
            entries.append(normalize(value))
    if not entries or len(entries) != len(set(entries)):
        raise ValueError("stable tree manifest must contain unique paths")
    return set(entries)


def commit_paths(root: Path, commit: str) -> dict[str, tuple[str, str]]:
    output = git(root, "ls-tree", "-r", "-z", "--full-tree", commit)
    paths: dict[str, tuple[str, str]] = {}
    for record in output.split(b"\0"):
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, kind, oid = metadata.decode("ascii").split(" ")
        path = raw_path.decode("utf-8")
        if mode not in ("100644", "100755", "120000") or kind != "blob":
            raise ValueError(f"unsupported stable tree entry: {path}")
        paths[path] = (mode, oid)
    return paths


def check(root: Path, commit: str, manifest_path: str, strict: bool = False) -> None:
    manifest_path = normalize(manifest_path)
    commit = git(root, "rev-parse", f"{commit}^{{commit}}").decode().strip()
    tree = commit_paths(root, commit)
    paths = set(tree)
    if manifest_path not in paths:
        raise ValueError("stable tree manifest is missing from selected commit")
    selected = manifest_paths(git(root, "show", f"{commit}:{manifest_path}"))
    missing = sorted(selected - paths)
    unlisted = sorted(paths - selected)
    ignored_prefixes = ("docs/plan/", "docs/pre-do/", "docs/design/plan/")
    ignored_exact = {
        "AGENTS.md", "CLAUDE.md", "spec/devlog.md",
        "docs/Telegram 多租户内容监控与分发平台.md",
        ".github/workflows/docker-build-validation.yml",
        ".github/workflows/docker-publish.yml",
        "compose.acceptance.override.yaml",
    }
    optional_development_exact = {".github/workflows/binary-release.yml"}
    unexpected = sorted(unlisted) if strict else sorted(
        path for path in unlisted
        if not path.startswith(ignored_prefixes)
        and not (path.startswith("scripts/ai-driver/") and path not in {
            "scripts/ai-driver/ai-run.sh", "scripts/ai-driver/ai-gates.sh",
            "scripts/ai-driver/check-regressions.py",
        })
        and path not in optional_development_exact
        and path not in ignored_exact
    )
    forbidden = sorted(
        path for path in selected
        if path.startswith(ignored_prefixes)
        or (path.startswith("scripts/ai-driver/") and path not in {
            "scripts/ai-driver/ai-run.sh",
            "scripts/ai-driver/ai-gates.sh",
            "scripts/ai-driver/check-regressions.py",
        })
        or path in ignored_exact
    )
    invalid_links = []
    for path in sorted(selected):
        mode, _ = tree[path]
        if mode != "120000":
            continue
        target = git(root, "show", f"{commit}:{path}").decode("utf-8", "strict")
        target_path = PurePosixPath(target)
        if target_path.is_absolute() or "\\" in target or "\x00" in target:
            invalid_links.append(f"{path} -> {target}")
            continue
        resolved = PurePosixPath(path).parent.joinpath(target_path)
        parts = []
        escapes = False
        for part in resolved.parts:
            if part in ("", "."):
                continue
            if part == "..":
                if not parts:
                    escapes = True
                    break
                parts.pop()
            else:
                parts.append(part)
        normalized = "/".join(parts)
        if escapes or normalized not in selected:
            invalid_links.append(f"{path} -> {target}")
    broken_links = []
    for path in sorted(selected):
        if not path.lower().endswith((".md", ".markdown")):
            continue
        content = git(root, "show", f"{commit}:{path}").decode("utf-8", "replace")
        for raw_target in re.findall(r"\]\(([^)]+)\)", content):
            target = raw_target.strip().split(None, 1)[0].strip("<>")
            if not target or target.startswith(("#", "//")) or "://" in target or target.startswith("mailto:"):
                continue
            target = unquote(target.split("#", 1)[0])
            if not target:
                continue
            resolved = PurePosixPath(path).parent.joinpath(target)
            parts = []
            for part in resolved.parts:
                if part in ("", "."):
                    continue
                if part == "..":
                    if not parts:
                        broken_links.append(f"{path} -> {raw_target}")
                        parts = []
                        break
                    parts.pop()
                else:
                    parts.append(part)
            else:
                destination = "/".join(parts)
                if (destination not in selected
                        and not any(item.startswith(destination.rstrip("/") + "/") for item in selected)):
                    broken_links.append(f"{path} -> {raw_target}")
    if missing or unexpected or forbidden or broken_links or invalid_links:
        for label, values in (("missing", missing), ("unexpected", unexpected), ("forbidden", forbidden), ("broken_links", sorted(set(broken_links))), ("invalid_links", invalid_links)):
            if values:
                print(f"{label}:\n" + "\n".join(values), file=sys.stderr)
        raise ValueError("stable tree does not match reviewed manifest")
    print(f"stable_tree=pass commit={commit} files={len(selected)} manifest={manifest_path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--commit", required=True)
    parser.add_argument("--manifest-path", required=True)
    parser.add_argument("--strict", action="store_true", help="reject every path outside the manifest")
    args = parser.parse_args()
    try:
        check(args.repository.resolve(strict=True), args.commit, args.manifest_path, strict=args.strict)
    except (OSError, UnicodeError, ValueError) as error:
        print(f"stable tree check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
