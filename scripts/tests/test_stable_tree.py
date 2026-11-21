from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
CHECKER = ROOT / "scripts" / "check-stable-tree.py"


def git(root: Path, *arguments: str) -> str:
    return subprocess.check_output(("git", "-C", str(root), *arguments), text=True).strip()


class StableTreeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="stable-tree-")
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        git(self.root, "init", "-q")
        git(self.root, "config", "user.name", "Stable Tree Test")
        git(self.root, "config", "user.email", "stable-tree@example.invalid")
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
        (self.root / "stable-tree.manifest").write_text(
            "src/main.rs\nstable-tree.manifest\n", encoding="utf-8"
        )
        self.commit()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def commit(self) -> None:
        git(self.root, "add", "--all")
        git(self.root, "-c", "user.name=Stable Tree Test", "-c",
            "user.email=stable-tree@example.invalid", "commit", "--allow-empty", "-qm", "fixture")

    def run_check(self) -> subprocess.CompletedProcess[str]:
        return self.run_check_with()

    def run_check_with(self, *extra: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            (sys.executable, str(CHECKER), "--repository", str(self.root),
             "--commit", git(self.root, "rev-parse", "HEAD"),
             "--manifest-path", "stable-tree.manifest", *extra),
            text=True, capture_output=True, check=False,
        )

    def test_exact_manifest_passes(self) -> None:
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_markdown_directory_link_passes_when_directory_has_allowlisted_files(self) -> None:
        (self.root / "README.md").write_text("[source](src/)\n", encoding="utf-8")
        (self.root / "stable-tree.manifest").write_text(
            "README.md\nsrc/main.rs\nstable-tree.manifest\n", encoding="utf-8"
        )
        self.commit()
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unlisted_or_missing_path_fails(self) -> None:
        (self.root / "src/extra.rs").write_text("// extra\n", encoding="utf-8")
        self.commit()
        self.assertNotEqual(self.run_check().returncode, 0)
        (self.root / "stable-tree.manifest").write_text(
            "src/main.rs\nstable-tree.manifest\nsrc/extra.rs\n", encoding="utf-8"
        )
        (self.root / "src/extra.rs").unlink()
        self.commit()
        self.assertNotEqual(self.run_check().returncode, 0)

    def test_development_material_is_rejected(self) -> None:
        (self.root / "docs/plan").mkdir(parents=True)
        (self.root / "docs/plan/draft.md").write_text("draft\n", encoding="utf-8")
        (self.root / "stable-tree.manifest").write_text(
            "src/main.rs\nstable-tree.manifest\ndocs/plan/draft.md\n", encoding="utf-8"
        )
        self.commit()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden", result.stderr)

    def test_unlisted_development_paths_are_ignored_but_unknown_paths_fail(self) -> None:
        (self.root / "docs/plan").mkdir(parents=True)
        (self.root / "docs/plan/draft.md").write_text("draft\n", encoding="utf-8")
        self.commit()
        self.assertEqual(self.run_check().returncode, 0)
        (self.root / "surprise.txt").write_text("unknown\n", encoding="utf-8")
        self.commit()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("surprise.txt", result.stderr)

    def test_candidate_workflow_is_not_an_allowed_excluded_path(self) -> None:
        (self.root / ".github/workflows").mkdir(parents=True)
        (self.root / ".github/workflows/binary-candidate.yml").write_text("name: candidate\\n", encoding="utf-8")
        self.commit()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("binary-candidate.yml", result.stderr)

    def test_stable_release_workflow_must_be_explicitly_allowlisted(self) -> None:
        (self.root / ".github/workflows").mkdir(parents=True)
        (self.root / ".github/workflows/binary-release.yml").write_text("name: release\n", encoding="utf-8")
        self.commit()
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_check_with("--strict")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("binary-release.yml", result.stderr)
        (self.root / "stable-tree.manifest").write_text(
            ".github/workflows/binary-release.yml\nsrc/main.rs\nstable-tree.manifest\n", encoding="utf-8"
        )
        self.commit()
        self.assertEqual(self.run_check_with("--strict").returncode, 0)

    def test_stable_release_trigger_and_permissions_are_scoped(self) -> None:
        workflow = (ROOT / ".github/workflows/binary-release.yml").read_text(encoding="utf-8")
        self.assertIn("v*.*.*", workflow)
        self.assertNotIn("workflow_dispatch:", workflow)
        self.assertIn("contents: read", workflow)
        self.assertIn("checks: read", workflow)
        self.assertIn("contents: write", workflow)
        self.assertIn("actions: read", workflow)
        self.assertIn("id-token: write", workflow)
        self.assertNotIn("--clobber", workflow)
        self.assertIn("stable release requires a newly created tag", workflow)
        self.assertIn("refusing to overwrite", workflow)

    def test_source_snapshot_ci_push_scope_is_read_only_and_task_limited(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("branches: [main, dev, 'codex/pre014-main-*']", workflow)
        self.assertNotIn("packages: write", workflow)
        self.assertNotIn("push: true", workflow)

    def test_strict_snapshot_rejects_even_approved_development_extras(self) -> None:
        (self.root / "docs/plan").mkdir(parents=True)
        (self.root / "docs/plan/draft.md").write_text("draft\n", encoding="utf-8")
        self.commit()
        result = self.run_check_with("--strict")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("docs/plan/draft.md", result.stderr)

    def test_markdown_link_to_excluded_file_fails(self) -> None:
        (self.root / "README.md").write_text("[plan](docs/plan/missing.md)\n", encoding="utf-8")
        (self.root / "docs/plan").mkdir(parents=True)
        (self.root / "docs/plan/missing.md").write_text("excluded\n", encoding="utf-8")
        manifest = self.root / "stable-tree.manifest"
        manifest.write_text("src/main.rs\nREADME.md\nstable-tree.manifest\n", encoding="utf-8")
        self.commit()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("broken_links", result.stderr)

    def test_internal_allowlisted_symlink_passes_and_escape_fails(self) -> None:
        (self.root / "src/link.rs").symlink_to("main.rs")
        (self.root / "stable-tree.manifest").write_text(
            "src/main.rs\nsrc/link.rs\nstable-tree.manifest\n", encoding="utf-8"
        )
        self.commit()
        self.assertEqual(self.run_check().returncode, 0)
        (self.root / "src/link.rs").unlink()
        (self.root / "src/link.rs").symlink_to("../../outside")
        self.commit()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid_links", result.stderr)


if __name__ == "__main__":
    unittest.main()
