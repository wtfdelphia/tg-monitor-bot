from __future__ import annotations

import hashlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "stable-source-archive.py"
SPEC = importlib.util.spec_from_file_location("stable_source_archive", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(("git", "-C", str(root), *args), text=True).strip()


class StableSourceArchiveTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="stable-source-archive-")
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        git(self.root, "init", "-q")
        git(self.root, "config", "user.name", "Archive Test")
        git(self.root, "config", "user.email", "archive@example.invalid")
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
        (self.root / "README.md").write_text("stable\n", encoding="utf-8")
        (self.root / "README.dev.md").write_text("exclude\n", encoding="utf-8")
        self.manifest = self.root / "manifest.txt"
        self.manifest.write_text("README.md\nsrc/main.rs\nmanifest.txt\n", encoding="utf-8")
        git(self.root, "add", ".")
        git(self.root, "commit", "-qm", "fixture")
        self.commit = git(self.root, "rev-parse", "HEAD")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def run_archive(self, output: Path | None = None, commit: str | None = None,
                    manifest_path: str = "manifest.txt") -> subprocess.CompletedProcess[str]:
        output = output or (Path(self.temp.name) / "out.tar.gz")
        return subprocess.run(
            (
                sys.executable, str(SCRIPT),
                "--repository", str(self.root),
                "--commit", commit or self.commit,
                "--manifest-path", manifest_path,
                "--output", str(output),
                "--source-date-epoch", "1700000000",
            ),
            cwd=self.root,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_archive_is_deterministic_and_contains_exact_manifest(self) -> None:
        first = Path(self.temp.name) / "first.tar.gz"
        second = Path(self.temp.name) / "second.tar.gz"
        self.assertEqual(self.run_archive(output=first).returncode, 0)
        self.assertEqual(self.run_archive(output=second).returncode, 0)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first, "r:gz") as archive:
            names = archive.getnames()
        self.assertEqual(sorted(names), sorted([
            f"tg-monitor-bot-source-{self.commit}/README.md",
            f"tg-monitor-bot-source-{self.commit}/SOURCE-MANIFEST.sha256",
            f"tg-monitor-bot-source-{self.commit}/manifest.txt",
            f"tg-monitor-bot-source-{self.commit}/src/main.rs",
        ]))
        self.assertNotIn(f"tg-monitor-bot-source-{self.commit}/README.dev.md", names)
        listing = subprocess.check_output((
            sys.executable, str(SCRIPT), "--repository", str(self.root), "--commit", self.commit,
            "--manifest-path", "manifest.txt", "--source-date-epoch", "1700000000", "--list",
        ), cwd=self.root, text=True).splitlines()
        self.assertEqual(sorted(names), sorted(listing))

    def test_manifest_path_is_recorded_in_audit_metadata(self) -> None:
        result=self.run_archive(manifest_path="manifest.txt")
        self.assertEqual(result.returncode,0,result.stderr)
        output=Path(self.temp.name)/"out.tar.gz"
        with tarfile.open(output,"r:gz") as archive:
            metadata=archive.extractfile(f"tg-monitor-bot-source-{self.commit}/SOURCE-MANIFEST.sha256")
            assert metadata is not None
            content=metadata.read().decode()
        self.assertIn("manifest_path=manifest.txt",content)
        self.assertIn(f"source_commit={self.commit}",content)

    def test_manifest_rejects_parent_traversal_and_missing_paths(self) -> None:
        self.assertRaises(ValueError, MODULE.normalize_path, "../secret")
        self.manifest.write_text("README.md\nmanifest.txt\nabsent\n", encoding="utf-8")
        git(self.root, "add", "manifest.txt")
        git(self.root, "commit", "-qm", "commit missing-path manifest")
        self.commit = git(self.root, "rev-parse", "HEAD")
        result = self.run_archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest entry does not exist", result.stderr)

    def test_symlink_entries_are_rejected(self) -> None:
        (self.root / "link").symlink_to("README.md")
        self.manifest.write_text("README.md\nmanifest.txt\nlink\n", encoding="utf-8")
        git(self.root, "add", "manifest.txt", "link")
        git(self.root, "commit", "-qm", "add symlink")
        self.commit = git(self.root, "rev-parse", "HEAD")
        result = self.run_archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported tree entry", result.stderr)

    def test_dirty_worktree_content_is_never_archived(self) -> None:
        (self.root / "untracked.txt").write_text("not in commit", encoding="utf-8")
        output = Path(self.temp.name) / "dirty-tree.tar.gz"
        result = self.run_archive(output=output)
        self.assertEqual(result.returncode, 0, result.stderr)
        with tarfile.open(output, "r:gz") as archive:
            self.assertNotIn(f"tg-monitor-bot-source-{self.commit}/untracked.txt", archive.getnames())

    def test_output_is_not_overwritten(self) -> None:
        output = Path(self.temp.name) / "existing.tar.gz"
        output.write_bytes(b"keep")
        result = self.run_archive(output=output)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output.read_bytes(), b"keep")

    def test_manifest_allows_only_exact_paths(self) -> None:
        manifest = self.root / "manifest.txt"
        manifest.write_text("README.md\nsrc\n", encoding="utf-8")
        git(self.root, "add", "manifest.txt")
        git(self.root, "commit", "-qm", "add directory glob to manifest")
        self.commit = git(self.root, "rev-parse", "HEAD")
        result = self.run_archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest entry does not exist as an exact file", result.stderr)

    def test_gzip_header_has_no_source_filename_or_wall_clock(self) -> None:
        output = Path(self.temp.name) / "header.tar.gz"
        result = self.run_archive(output=output)
        self.assertEqual(result.returncode, 0, result.stderr)
        header = output.read_bytes()[:10]
        self.assertEqual(header[3], 0)
        self.assertEqual(int.from_bytes(header[4:8], "little"), 1700000000)


if __name__ == "__main__":
    unittest.main()
