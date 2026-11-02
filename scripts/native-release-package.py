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
    parser.add_argument("--channel", choices=("stable", "candidate"), default="stable")
    parser.add_argument("--candidate-run-id")
    parser.add_argument("--candidate-attempt")
    parser.add_argument("--candidate-ci-run-id")
    parser.add_argument("--candidate-run-url")
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
    if args.channel == "candidate":
        if not re.fullmatch(r"[1-9][0-9]*", args.candidate_run_id or ""):
            raise SystemExit("候选包必须提供有效的 workflow run id")
        if not re.fullmatch(r"[1-9][0-9]*", args.candidate_attempt or ""):
            raise SystemExit("候选包必须提供有效的 workflow run attempt")
        if not re.fullmatch(r"[1-9][0-9]*", args.candidate_ci_run_id or ""):
            raise SystemExit("候选包必须记录成功的 CI workflow run id")
        expected_run_url = (
            "https://github.com/wtfdelphia/tg-monitor-bot/actions/runs/"
            f"{args.candidate_run_id}"
        )
        if args.candidate_run_url != expected_run_url:
            raise SystemExit("候选包必须记录本仓库对应的 workflow run URL")
        if not re.fullmatch(r"[0-9a-f]{40}", args.source_commit):
            raise SystemExit("候选包来源必须是完整 Git commit SHA")
    elif any((args.candidate_run_id, args.candidate_attempt, args.candidate_ci_run_id, args.candidate_run_url)):
        raise SystemExit("稳定包不能携带候选 run 标识")
    if args.output.exists() and any(args.output.iterdir()):
        raise SystemExit(f"输出目录非空：{args.output}")
    args.output.mkdir(parents=True, exist_ok=True)

    short_commit = args.source_commit[:12]
    if args.channel == "candidate":
        version = f"{args.version}-candidate.{args.candidate_run_id}.{args.candidate_attempt}.{short_commit}"
        release_tag = f"candidate-{args.candidate_run_id}-a{args.candidate_attempt}-{short_commit}"
    else:
        version = args.version
        release_tag = f"v{version}"
    archive_name = f"tgm-{version}-linux-x86_64-gnu.tar.gz"
    with tempfile.TemporaryDirectory(prefix="tgm-native-package-") as temporary:
        staging = Path(temporary)
        (staging / "bin").mkdir()
        (staging / "deploy" / "systemd").mkdir(parents=True)
        (staging / "scripts").mkdir()
        shutil.copy2(binary, staging / "bin" / "tgm")
        (staging / "bin" / "tgm").chmod(0o755)
        shutil.copy2(sbom, staging / "sbom.cdx.json")
        (staging / "VERSION").write_text(f"{version}\n", encoding="utf-8")
        shutil.copy2(repository / "scripts" / "install-native.sh", staging / "scripts")
        (staging / "scripts" / "install-native.sh").chmod(0o755)
        shutil.copy2(repository / "scripts" / "native-release-preflight.sh", staging / "scripts")
        (staging / "scripts" / "native-release-preflight.sh").chmod(0o755)
        if args.channel == "candidate":
            shutil.copy2(repository / "scripts" / "verify-native-candidate.py", staging / "scripts")
            (staging / "scripts" / "verify-native-candidate.py").chmod(0o755)
        shutil.copy2(repository / "deploy" / "native" / "README.md", staging / "README.md")
        readme = (staging / "README.md").read_text(encoding="utf-8")
        readme = readme.replace("@VERSION@", version)
        readme = readme.replace("@SOURCE_COMMIT@", args.source_commit)
        readme = readme.replace("@TARGET@", args.target)
        if args.channel == "candidate":
            readme = readme.replace("@CHANNEL@", "candidate")
            readme = readme.replace(
                "@STABILITY_NOTICE@",
                "**CANDIDATE / NOT STABLE：仅供隔离的非生产验收，不得作为稳定版或生产发布使用。**",
            )
            candidate_distribution = (
                "候选 Pre-release 另附 `candidate-SHA256SUMS` 及签名 bundle、"
                "`candidate-provenance.json` 及签名 bundle、`candidate-BUILDINFO.json` 和 "
                "`candidate-sbom.cdx.json`。清单签名绑定归档、构建信息与 SBOM；安装器另核验来源证明。\n\n"
                "首次运行候选发布前，需将 `.github/workflows/binary-candidate.yml` 合入 GitHub 默认分支，"
                "再从 Actions 选择 `dev` 触发 `workflow_dispatch`。这不代表发布稳定版。"
            )
            candidate_install = (
                "候选包必须显式指定 `--candidate --accept-candidate`，仅供隔离的非生产验收。"
                "只允许非 root 用户安装到候选目录，不接受 `--app-services`，不切换稳定命令链接。\n\n"
                "候选来源验证需要预先安装 Python 3；安装器不会下载解释器。\n\n"
                "先验证清单签名和其中所有文件摘要，再从归档读取安装器：\n\n"
                "```bash\n"
                "cosign verify-blob --offline \\\n"
                "  --bundle candidate-SHA256SUMS.sigstore.json \\\n"
                "  --certificate-identity 'https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-candidate.yml@refs/heads/dev' \\\n"
                "  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \\\n"
                "  candidate-SHA256SUMS\n"
                "sha256sum -c candidate-SHA256SUMS\n"
                "tar -xOzf tgm-@VERSION@-linux-x86_64-gnu.tar.gz ./scripts/install-native.sh > install-native.sh\n"
                "bash install-native.sh --user --candidate --accept-candidate \\\n"
                "  --archive tgm-@VERSION@-linux-x86_64-gnu.tar.gz \\\n"
                "  --checksums candidate-SHA256SUMS \\\n"
                "  --bundle candidate-SHA256SUMS.sigstore.json \\\n"
                "  --provenance candidate-provenance.json \\\n"
                "  --provenance-bundle candidate-provenance.sigstore.json \\\n"
                "  --buildinfo candidate-BUILDINFO.json \\\n"
                "  --sbom candidate-sbom.cdx.json\n"
                "```\n\n"
                "安装器会核验 provenance 签名身份、来源提交和归档摘要。验收结束后，确认候选版本未被进程使用，"
                "再删除对应的 `~/.local/opt/tgm/candidates/<VERSION>`；安装器不自动清理，也不安装或修改 systemd unit。"
            )
            readme = readme.replace(
                "@CANDIDATE_DISTRIBUTION@",
                candidate_distribution,
            )
            readme = readme.replace(
                "@CANDIDATE_INSTALL@",
                candidate_install,
            )
        else:
            readme = readme.replace(
                "\n\n发布通道：`@CHANNEL@`\n\n@STABILITY_NOTICE@\n\n",
                "\n\n",
            )
            readme = readme.replace("@CANDIDATE_DISTRIBUTION@", "")
            readme = readme.replace("@CANDIDATE_INSTALL@", "")
        (staging / "README.md").write_text(readme, encoding="utf-8")
        shutil.copytree(repository / "deploy" / "systemd", staging / "deploy" / "systemd", dirs_exist_ok=True)

        buildinfo = {
            "artifact": archive_name,
            "version": version,
            "source_commit": args.source_commit,
            "target": args.target,
            "rust_toolchain": args.rust_toolchain,
            "builder_image": args.builder_image,
            "source_date_epoch": args.source_date_epoch,
            "cargo_lock_sha256": sha256(repository / "Cargo.lock"),
            "openssl": "vendored via josekit feature",
        }
        if args.channel == "candidate":
            buildinfo.update({
                "channel": "candidate",
                "stable": False,
                "candidate_run_id": int(args.candidate_run_id),
                "candidate_attempt": int(args.candidate_attempt),
                "ci_run_id": int(args.candidate_ci_run_id),
                "workflow_run_url": args.candidate_run_url,
                "release_tag": release_tag,
            })
        (staging / "BUILDINFO.json").write_text(
            json.dumps(buildinfo, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        write_checksums(staging)
        set_tree_mtime(staging, args.source_date_epoch)

        package_root = args.output / f"tgm-{version}-linux-x86_64-gnu"
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

    print(f"发布通道：{args.channel}")
    print(f"发布包：{args.output / archive_name}")
    print(f"发布包 SHA-256：{sha256(args.output / archive_name)}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        print(f"打包命令失败：{error}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
