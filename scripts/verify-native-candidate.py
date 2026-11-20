#!/usr/bin/env python3
"""Verify candidate release metadata and its signed SLSA provenance."""

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path


REPOSITORY = "wtfdelphia/tg-monitor-bot"
OIDC_ISSUER = "https://token.actions.githubusercontent.com"
IDENTITY = (
    "https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/"
    "binary-candidate.yml@refs/heads/dev"
)


def fail(message: str) -> None:
    raise SystemExit(f"候选来源验证失败：{message}")


def read_json(path: Path) -> dict:
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream)
    except (OSError, json.JSONDecodeError) as error:
        fail(f"无法读取 JSON：{path}: {error}")
    if not isinstance(value, dict):
        fail(f"JSON 根节点必须是对象：{path}")
    return value


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_signature(path: Path, bundle: Path) -> None:
    subprocess.run(
        [
            "cosign", "verify-blob", "--offline", "--bundle", str(bundle),
            "--certificate-identity", IDENTITY,
            "--certificate-oidc-issuer", OIDC_ISSUER, str(path),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="验证 TGM 候选包来源")
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--checksums", type=Path, required=True)
    parser.add_argument("--checksum-bundle", type=Path, required=True)
    parser.add_argument("--provenance", type=Path, required=True)
    parser.add_argument("--provenance-bundle", type=Path, required=True)
    parser.add_argument("--buildinfo", type=Path, required=True)
    parser.add_argument("--sbom", type=Path, required=True)
    parser.add_argument("--package-buildinfo", type=Path, required=True)
    parser.add_argument("--package-sbom", type=Path, required=True)
    parser.add_argument("--version", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    buildinfo = read_json(args.buildinfo)
    provenance = read_json(args.provenance)
    if buildinfo.get("channel") != "candidate" or buildinfo.get("stable") is not False:
        fail("BUILDINFO 未标记为非稳定候选")

    source_commit = buildinfo.get("source_commit")
    run_id = buildinfo.get("candidate_run_id")
    attempt = buildinfo.get("candidate_attempt")
    ci_run_id = buildinfo.get("ci_run_id")
    if not isinstance(source_commit, str) or not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        fail("BUILDINFO 中源码提交不是完整 SHA")
    if (type(run_id) is not int or run_id < 1 or type(attempt) is not int or attempt < 1
            or type(ci_run_id) is not int or ci_run_id < 1):
        fail("BUILDINFO 中候选运行标识无效")

    version = args.version
    version_pattern = rf"^[0-9]+\.[0-9]+\.[0-9]+-candidate\.{run_id}\.{attempt}\.{source_commit[:12]}$"
    if not re.fullmatch(version_pattern, version):
        fail("候选 VERSION 与 run、attempt 或源码提交不匹配")
    release_tag = f"candidate-{run_id}-a{attempt}-{source_commit[:12]}"
    if buildinfo.get("release_tag") != release_tag:
        fail("候选 tag 与 BUILDINFO 不匹配")
    expected_run_url = f"https://github.com/{REPOSITORY}/actions/runs/{run_id}"
    if buildinfo.get("workflow_run_url") != expected_run_url:
        fail("候选 workflow run URL 与 BUILDINFO 不匹配")
    if buildinfo.get("version") != version or buildinfo.get("artifact") != args.archive.name:
        fail("候选 VERSION、归档名与 BUILDINFO 不匹配")
    if buildinfo.get("target") != "x86_64-unknown-linux-gnu":
        fail("BUILDINFO 目标平台不是 Linux x86-64 glibc")
    builder_image = buildinfo.get("builder_image")
    if not isinstance(builder_image, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", builder_image):
        fail("BUILDINFO 中构建器镜像 ID 无效")

    expected_archive_digest = sha256(args.archive)
    if args.buildinfo.read_bytes() != args.package_buildinfo.read_bytes():
        fail("包内 BUILDINFO 与 Release BUILDINFO 不一致")
    if args.package_sbom.read_bytes() != args.sbom.read_bytes():
        fail("包内 SBOM 与 Release SBOM 不一致")
    checksum_lines = args.checksums.read_text(encoding="ascii").splitlines()
    expected_files = {
        args.archive.name: expected_archive_digest,
        args.buildinfo.name: sha256(args.buildinfo),
        args.sbom.name: sha256(args.sbom),
    }
    checksum_entries = {}
    for line in checksum_lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if match is None or match.group(2) in checksum_entries:
            fail("候选 SHA256SUMS 格式无效或包含重复路径")
        checksum_entries[match.group(2)] = match.group(1)
    if checksum_entries != expected_files:
        fail("候选 SHA256SUMS 必须且只能绑定归档、BUILDINFO 和 SBOM")

    verify_signature(args.checksums, args.checksum_bundle)
    verify_signature(args.provenance, args.provenance_bundle)

    if provenance.get("_type") != "https://in-toto.io/Statement/v1":
        fail("provenance 不是 in-toto Statement v1")
    if provenance.get("predicateType") != "https://slsa.dev/provenance/v1":
        fail("provenance predicate type 不是 SLSA provenance v1")
    subjects = provenance.get("subject")
    if not isinstance(subjects, list) or len(subjects) != 1:
        fail("provenance 必须且只能有一个归档 subject")
    subject = subjects[0]
    if not isinstance(subject, dict) or not isinstance(subject.get("digest"), dict):
        fail("provenance subject 结构无效")
    if subject.get("name") != args.archive.name or subject["digest"].get("sha256") != expected_archive_digest:
        fail("provenance subject 与归档摘要不匹配")

    predicate = provenance.get("predicate")
    if not isinstance(predicate, dict) or not isinstance(predicate.get("buildDefinition"), dict):
        fail("provenance predicate/buildDefinition 结构无效")
    definition = predicate["buildDefinition"]
    if definition.get("buildType") != f"https://github.com/{REPOSITORY}/.github/workflows/binary-candidate.yml":
        fail("provenance build workflow 不匹配")
    parameters = definition.get("externalParameters")
    if not isinstance(parameters, dict):
        fail("provenance externalParameters 结构无效")
    if (parameters.get("candidate_tag") != release_tag or parameters.get("candidate_version") != version
            or str(parameters.get("candidate_run_id")) != str(run_id)
            or str(parameters.get("candidate_attempt")) != str(attempt)):
        fail("provenance candidate tag 或 version 不匹配")
    if parameters.get("channel") != "candidate" or str(parameters.get("ci_run_id")) != str(ci_run_id):
        fail("provenance channel 或成功 CI run id 与 BUILDINFO 不匹配")
    if parameters.get("target") != buildinfo.get("target"):
        fail("provenance target 与 BUILDINFO 不匹配")
    if parameters.get("builder_image") != buildinfo.get("builder_image"):
        fail("provenance builder image 与 BUILDINFO 不匹配")
    dependencies = definition.get("resolvedDependencies")
    if not isinstance(dependencies, list) or len(dependencies) != 1:
        fail("provenance resolvedDependencies 缺失或数量错误")
    dependency = dependencies[0]
    if not isinstance(dependency, dict) or not isinstance(dependency.get("digest"), dict):
        fail("provenance resolved dependency 结构无效")
    if dependency["digest"].get("gitCommit") != source_commit:
        fail("provenance 源码提交与 BUILDINFO 不匹配")
    if dependency.get("uri") != f"git+https://github.com/{REPOSITORY}@refs/heads/dev":
        fail("provenance 源码引用不是仓库 dev 分支")

    run_details = predicate.get("runDetails")
    if not isinstance(run_details, dict) or not isinstance(run_details.get("builder"), dict):
        fail("provenance runDetails 结构无效")
    run_id_text = str(run_id)
    if run_details["builder"].get("id") != f"https://github.com/{REPOSITORY}/actions/runs/{run_id_text}":
        fail("provenance builder run id 与 BUILDINFO 不匹配")
    metadata = run_details.get("metadata", {})
    if not isinstance(metadata, dict):
        fail("provenance metadata 结构无效")
    expected_invocation = (
        f"https://github.com/{REPOSITORY}/actions/runs/{run_id_text}/attempts/{attempt}"
    )
    if metadata.get("invocationId") != expected_invocation:
        fail("provenance invocation id 与 BUILDINFO 不匹配")

    print(f"候选来源验证通过：{release_tag} commit={source_commit} sha256={expected_archive_digest}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"候选来源验证失败：{error}", file=sys.stderr)
        raise SystemExit(1) from error
