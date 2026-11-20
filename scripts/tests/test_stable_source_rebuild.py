from __future__ import annotations

import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-stable-source-rebuild.py"
SPEC = importlib.util.spec_from_file_location("stable_source_rebuild", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class StableSourceRebuildTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="stable-source-rebuild-")
        self.root = Path(self.temp.name)
        self.commit = "a" * 40
        self.prefix = f"tg-monitor-bot-source-{self.commit}/"
        self.manifest_path = "scripts/stable-source-archive.manifest"
        self.manifest = b"README.md\nscripts/stable-source-archive.manifest\n"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_archive(self, extras: tuple[tuple[str, bytes, str], ...] = (),
                      manifest_content: bytes | None = None) -> Path:
        path = self.root / "source.tar.gz"
        entries = [
            ("README.md", b"readme\n", "file"),
            (self.manifest_path, manifest_content if manifest_content is not None else self.manifest, "file"),
            ("SOURCE-MANIFEST.sha256", b"", "metadata"),
        ] + list(extras)
        metadata = (
            f"manifest_sha256={hashlib.sha256(self.manifest).hexdigest()}\n"
            f"manifest_path={self.manifest_path}\nsource_commit={self.commit}\n"
        ).encode()
        with tarfile.open(path, "w:gz") as archive:
            for name, content, kind in entries:
                info = tarfile.TarInfo(self.prefix + name)
                if kind == "symlink":
                    info.type = tarfile.SYMTYPE
                    info.linkname = "README.md"
                    archive.addfile(info)
                else:
                    payload = metadata if kind == "metadata" else content
                    info.size = len(payload)
                    archive.addfile(info, io.BytesIO(payload))
        return path

    def test_valid_archive_extracts(self) -> None:
        archive = self.write_archive()
        destination = self.root / "out"
        destination.mkdir()
        extracted = MODULE.verify_source_archive(archive, self.commit, destination)
        self.assertEqual((extracted / "README.md").read_bytes(), b"readme\n")

    def test_path_traversal_extra_member_duplicate_and_symlink_fail(self) -> None:
        cases = (
            (("../escape", b"bad", "file"),),
            (("unlisted", b"extra", "file"),),
            (("README.md", b"again", "file"),),
            (("link", b"", "symlink"),),
        )
        for extras in cases:
            with self.subTest(extras=extras):
                with self.assertRaises(ValueError):
                    MODULE.verify_source_archive(self.write_archive(extras), self.commit, self.root / "out")

    def test_manifest_tampering_fails(self) -> None:
        archive = self.write_archive(
            manifest_content=b"README.md\nscripts/stable-source-archive.manifest\nextra\n"
        )
        with self.assertRaises(ValueError):
            MODULE.verify_source_archive(archive, self.commit, self.root / "out")


if __name__ == "__main__":
    unittest.main()
