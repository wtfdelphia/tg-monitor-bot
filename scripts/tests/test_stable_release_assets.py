from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import hashlib
import io
import os
import subprocess
import tempfile
import tarfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("stable_release_assets", ROOT / "scripts/stable-release-assets.py")
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class StableReleaseAssetsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="stable-release-assets-")
        self.assets = Path(self.temp.name) / "assets"
        self.assets.mkdir()
        self.version = "0.1.0"
        self.commit = "a" * 40
        self.builder = "sha256:" + "b" * 64
        self.toolchain = "1.98.1-x86_64-unknown-linux-gnu"
        manifest = (ROOT / "scripts/stable-source-archive.manifest").read_bytes()
        manifest_paths = [
            line.strip() for line in manifest.decode("utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
        self.manifest_sha = hashlib.sha256(manifest).hexdigest()
        for name in MODULE.expected_payloads(self.assets, self.version):
            target = self.assets / name
            if name not in ("SOURCE-ARCHIVE-MANIFEST.txt", f"tgm-{self.version}-source.tar.gz",
                            f"tgm-{self.version}-linux-x86_64-gnu.tar.gz", "BUILDINFO.json"):
                target.write_bytes(f"payload:{name}\n".encode())
        (self.assets / "SOURCE-ARCHIVE-MANIFEST.txt").write_bytes(manifest)
        source_archive = self.assets / f"tgm-{self.version}-source.tar.gz"
        metadata = (
            f"manifest_sha256={self.manifest_sha}\nmanifest_path=scripts/stable-source-archive.manifest\n"
            f"source_commit={self.commit}\n"
        ).encode()
        with tarfile.open(source_archive, "w:gz") as archive:
            for name in manifest_paths + ["SOURCE-MANIFEST.sha256"]:
                content = manifest if name == "scripts/stable-source-archive.manifest" else (
                    metadata if name == "SOURCE-MANIFEST.sha256" else b"fixture source\n"
                )
                member = tarfile.TarInfo(f"tg-monitor-bot-source-{self.commit}/{name}")
                member.size = len(content)
                archive.addfile(member, io.BytesIO(content))
        (self.assets / "BUILDINFO.json").write_text(json.dumps({
            "version": self.version,
            "source_commit": self.commit,
            "builder_image": self.builder,
            "rust_toolchain": self.toolchain,
            "target": "x86_64-unknown-linux-gnu",
            "channel": "stable",
            "stable": True,
            "release_tag": f"v{self.version}",
            "artifact": f"tgm-{self.version}-linux-x86_64-gnu.tar.gz",
            "source_manifest_sha256": self.manifest_sha,
        }), encoding="utf-8")
        binary_archive = self.assets / f"tgm-{self.version}-linux-x86_64-gnu.tar.gz"
        package_files = {
            "BUILDINFO.json": (self.assets / "BUILDINFO.json").read_bytes(),
            "sbom.cdx.json": (self.assets / "sbom.cdx.json").read_bytes(),
            "VERSION": f"{self.version}\n".encode(),
            "bin/tgm": b"fixture binary\n",
        }
        package_files["SHA256SUMS"] = "".join(
            f"{hashlib.sha256(content).hexdigest()}  {name}\n"
            for name, content in sorted(package_files.items())
        ).encode()
        with tarfile.open(binary_archive, "w:gz") as archive:
            root_member = tarfile.TarInfo("./")
            root_member.type = tarfile.DIRTYPE
            archive.addfile(root_member)
            for directory in ("./bin/",):
                directory_member = tarfile.TarInfo(directory)
                directory_member.type = tarfile.DIRTYPE
                archive.addfile(directory_member)
            for name, content in package_files.items():
                member = tarfile.TarInfo("./" + name)
                member.size = len(content)
                archive.addfile(member, io.BytesIO(content))

    def tearDown(self) -> None:
        self.temp.cleanup()

    def prepare(self) -> None:
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "builder_image": self.builder,
            "rust_toolchain": self.toolchain,
            "repository": "wtfdelphia/tg-monitor-bot",
            "run_id": "12345",
            "source_manifest_sha256": self.manifest_sha,
        })()
        MODULE.prepare(namespace)

    def test_prepare_and_verify_bind_exact_payloads(self) -> None:
        self.prepare()
        provenance_path = self.assets / "provenance.json"
        provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
        provenance["predicate"]["runDetails"] = {
            "builder": {"id": "https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/12345"},
            "metadata": {"invocationId": "12345"},
        }
        provenance_path.write_text(json.dumps(provenance), encoding="utf-8")
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "repository": "wtfdelphia/tg-monitor-bot",
            "expected_run_id": "12345",
            "verify_signatures": False,
        })()
        self.write_bundles()
        MODULE.verify(namespace)

    def write_bundles(self) -> None:
        for name in ("SHA256SUMS.sigstore.json", "provenance.sigstore.json"):
            (self.assets / name).write_text("{}\n", encoding="utf-8")

    def test_tampered_payload_and_unexpected_asset_fail(self) -> None:
        self.prepare()
        self.write_bundles()
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "repository": "wtfdelphia/tg-monitor-bot",
            "verify_signatures": False,
        })()
        payload = self.assets / f"tgm-{self.version}-source.tar.gz"
        payload.write_bytes(b"tampered")
        with self.assertRaises(ValueError):
            MODULE.verify(namespace)
        payload.write_bytes(f"payload:{payload.name}\n".encode())
        (self.assets / "extra.txt").write_text("extra", encoding="utf-8")
        with self.assertRaises(ValueError):
            MODULE.verify(namespace)

    def test_provenance_wrong_commit_fails(self) -> None:
        self.prepare()
        self.write_bundles()
        path = self.assets / "provenance.json"
        provenance = json.loads(path.read_text(encoding="utf-8"))
        provenance["predicate"]["buildDefinition"]["resolvedDependencies"][0]["digest"]["gitCommit"] = "c" * 40
        path.write_text(json.dumps(provenance), encoding="utf-8")
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "repository": "wtfdelphia/tg-monitor-bot",
        })()
        with self.assertRaises(ValueError):
            MODULE.verify(namespace)

    def test_provenance_run_id_must_match_signing_run(self) -> None:
        self.prepare()
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "repository": "wtfdelphia/tg-monitor-bot",
            "expected_run_id": "99999",
            "verify_signatures": False,
        })()
        with self.assertRaises(ValueError):
            MODULE.verify(namespace)

    def test_provenance_manifest_or_stable_identity_mismatch_fails(self) -> None:
        self.prepare()
        self.write_bundles()
        path = self.assets / "provenance.json"
        original = json.loads(path.read_text(encoding="utf-8"))
        namespace = type("Args", (), {
            "assets": self.assets,
            "version": self.version,
            "source_commit": self.commit,
            "repository": "wtfdelphia/tg-monitor-bot",
        })()
        for mutate in (
            lambda value: value["predicate"]["buildDefinition"]["externalParameters"].update(
                source_manifest_sha256="0" * 64),
            lambda value: value["predicate"]["buildDefinition"]["externalParameters"].update(
                release_tag="v9.9.9"),
        ):
            with self.subTest(mutation=mutate):
                path.write_text(json.dumps(original), encoding="utf-8")
                provenance = json.loads(path.read_text(encoding="utf-8"))
                mutate(provenance)
                path.write_text(json.dumps(provenance), encoding="utf-8")
                with self.assertRaises(ValueError):
                    MODULE.verify(namespace)

    def test_signature_verification_uses_configured_identity_and_fails_closed(self) -> None:
        self.prepare()
        self.write_bundles()
        calls = Path(self.temp.name) / "calls"
        bindir = Path(self.temp.name) / "bin"
        bindir.mkdir()
        cosign = bindir / "cosign"
        cosign.write_text(
            "#!/usr/bin/env python3\nimport os, pathlib, sys\n"
            "pathlib.Path(os.environ['COSIGN_CALLS']).write_text(' '.join(sys.argv[1:]))\n"
            "raise SystemExit(int(os.environ.get('COSIGN_STATUS', '0')))\n",
            encoding="utf-8",
        )
        cosign.chmod(0o755)
        previous = os.environ.get("PATH", "")
        os.environ["PATH"] = f"{bindir}:{previous}"
        os.environ["COSIGN_CALLS"] = str(calls)
        try:
            namespace = type("Args", (), {
                "assets": self.assets, "version": self.version,
                "source_commit": self.commit, "repository": "wtfdelphia/tg-monitor-bot",
                "verify_signatures": True,
            })()
            MODULE.verify(namespace)
            self.assertIn("--certificate-identity-regexp", calls.read_text())
            os.environ["COSIGN_STATUS"] = "1"
            with self.assertRaises(subprocess.CalledProcessError):
                MODULE.verify(namespace)
        finally:
            os.environ.pop("COSIGN_CALLS", None)
            os.environ.pop("COSIGN_STATUS", None)
            os.environ["PATH"] = previous


if __name__ == "__main__":
    unittest.main()
