#!/usr/bin/env python3
"""Check G1 input readiness without printing credentials or executing an env file."""

import argparse
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import stat
import urllib.parse
import urllib.request


DISCOVERY = "https://oauth.telegram.org/.well-known/openid-configuration"
EXPECTED = {
    "issuer": "https://oauth.telegram.org",
    "authorization_endpoint": "https://oauth.telegram.org/auth",
    "token_endpoint": "https://oauth.telegram.org/token",
    "jwks_uri": "https://oauth.telegram.org/.well-known/jwks.json",
}


def private_file(path):
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("输入必须是当前用户拥有的普通文件")
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        raise ValueError("输入文件必须使用 0600 或更严格权限")
    return path.read_text(encoding="utf-8")


def read_env(path):
    values = {}
    for line in private_file(path).splitlines():
        fields = shlex.split(line, comments=True)
        if fields[:1] == ["export"]:
            fields = fields[1:]
        if not fields:
            continue
        if len(fields) != 1 or "=" not in fields[0]:
            raise ValueError("仅接受单行 KEY=value，不执行 Shell")
        key, value = fields[0].split("=", 1)
        if not key.isidentifier() or key in values:
            raise ValueError("变量名非法或重复")
        values[key] = value
    return values


def provided(value):
    return isinstance(value, str) and bool(value.strip()) and not value.startswith("REPLACE")


def check(values, tenant):
    if not isinstance(tenant, dict):
        raise ValueError("租户申请记录必须是 JSON 对象")
    issues = []
    for key in ("TGM_OIDC_BOT_ID", "TGM_OIDC_CLIENT_SECRET", "TGM_OIDC_ALG", "TGM_OIDC_REDIRECT_URL"):
        if not provided(values.get(key)):
            issues.append("缺少 " + key)
    client_id = values.get("TGM_OIDC_BOT_ID", "")
    if provided(client_id) and (not client_id.isascii() or not client_id.isdigit() or int(client_id) <= 0):
        issues.append("Client ID 必须为正整数")
    algorithm = values.get("TGM_OIDC_ALG")
    if provided(algorithm) and algorithm not in ("RS256", "ES256"):
        issues.append("需要 profile.id 的登录仅接受 RS256/ES256")
    redirect = values.get("TGM_OIDC_REDIRECT_URL", "")
    if provided(redirect):
        parsed = urllib.parse.urlsplit(redirect)
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password
                or parsed.query or parsed.fragment or parsed.path != "/auth/oidc/callback"):
            issues.append("回调必须为 HTTPS origin 下的 /auth/oidc/callback，无用户信息、查询串或片段")
        origin = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, "", "", ""))
        if values.get("TGM_OIDC_ALLOWED_ORIGIN") != origin:
            issues.append("Allowed origin 与回调 origin 不一致")
        if values.get("TGM_OIDC_ALLOWED_CALLBACK") != redirect:
            issues.append("Allowed callback 与完整回调 URI 不一致")
    if values.get("TGM_OIDC_BOTFATHER_CONFIRMED") != "yes":
        issues.append("尚未确认 BotFather Allowed URLs")
    for key in ("operator_reference", "tenant_reference", "applied_at", "evidence_reference", "reviewer"):
        if not provided(tenant.get(key)):
            issues.append("租户申请记录缺少 " + key)
    if tenant.get("result") != "pass" or tenant.get("applicant_is_tenant") is not True:
        issues.append("租户自行申请 api_id 尚未提供通过记录")
    if tenant.get("platform_credentials_reused") is not False:
        issues.append("尚未确认申请证据没有复用平台凭据")
    return issues


def tenant_evidence_available(tenant):
    reference = tenant.get("evidence_reference")
    if not provided(reference):
        return False
    path = Path(reference)
    if not path.is_absolute():
        return False
    try:
        metadata = path.lstat()
    except OSError:
        return False
    return (stat.S_ISREG(metadata.st_mode) and metadata.st_uid == os.getuid()
            and not stat.S_IMODE(metadata.st_mode) & 0o077 and metadata.st_size > 0)


def record_field(text, label):
    matches = re.findall(r"^" + re.escape(label) + r"[：:][ \t]*(.*)$", text, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError("复核字段缺失或重复")
    return matches[0].strip()


def application_date(value):
    if not isinstance(value, str):
        raise ValueError("申请日期必须为字符串")
    matched = re.fullmatch(r"([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})", value)
    if matched is None:
        raise ValueError("申请日期格式无效")
    return date(*(int(part) for part in matched.groups())).isoformat()


def review_tenant_evidence(tenant, signoff):
    if not tenant_evidence_available(tenant):
        raise ValueError("租户证据文件不满足私密输入要求")
    evidence_bytes = Path(tenant["evidence_reference"]).read_bytes()
    evidence_text = evidence_bytes.decode("utf-8")
    index_date = application_date(tenant["applied_at"])
    evidence_date = application_date(record_field(evidence_text, "申请日期"))
    evidence_hash = hashlib.sha256(evidence_bytes).hexdigest()
    signoff_hash = record_field(signoff, "目标租户证据 SHA-256")
    if re.fullmatch(r"[0-9a-fA-F]{64}", signoff_hash) is None:
        raise ValueError("签署材料的证据哈希格式无效")
    issues = []
    if index_date != evidence_date:
        issues.append("申请索引与脱敏原件的申请日期不一致，须由责任方确认")
    if evidence_hash != signoff_hash.lower():
        issues.append("当前脱敏原件与签署材料的证据哈希不一致，须由责任方确认替代关系")
    return {
        "status": "blocked-confirmation" if issues else "consistent-pending-review",
        "index_applied_at": index_date,
        "evidence_applied_at": evidence_date,
        "current_evidence_sha256": evidence_hash,
        "signoff_evidence_sha256": signoff_hash.lower(),
        "issues": issues,
        "formal_signoff": "not-assessed",
    }


def provider_summary():
    with urllib.request.urlopen(DISCOVERY, timeout=20) as response:
        data = json.loads(response.read(131073))
    if not isinstance(data, dict) or any(data.get(key) != value for key, value in EXPECTED.items()):
        raise ValueError("provider discovery 与固定 Telegram 端点不一致")
    if "S256" not in data.get("code_challenge_methods_supported", []):
        raise ValueError("provider 未声明 S256")
    with urllib.request.urlopen(EXPECTED["jwks_uri"], timeout=20) as response:
        key_set = json.loads(response.read(1048577))
    keys = key_set.get("keys") if isinstance(key_set, dict) else None
    if (not isinstance(keys, list) or not keys
            or any(not isinstance(key, dict) or any(field in key for field in ("d", "p", "q", "k"))
                   for key in keys)):
        raise ValueError("provider JWKS 为空或包含私钥材料")
    return {
        "discovery": "pass", "jwks_fetch": "pass", "jwks_keys": len(keys),
        "client_auth_methods": data.get("token_endpoint_auth_methods_supported", []),
        "algorithms": data.get("id_token_signing_alg_values_supported", []),
        "real_login": "not-run", "provider_rotation": "not-run",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env-file", type=Path, required=True)
    parser.add_argument("--tenant-record", type=Path, required=True)
    parser.add_argument("--signoff-file", type=Path, help="只读比对脱敏申请日期和既有签署材料哈希，不代替责任方确认")
    parser.add_argument("--provider", action="store_true", help="只读查询公开 discovery/JWKS，不发送凭据")
    args = parser.parse_args()
    try:
        values = read_env(args.env_file)
        tenant = json.loads(private_file(args.tenant_record))
        if not isinstance(tenant, dict):
            raise ValueError("租户申请记录必须为 JSON object")
        issues = check(values, tenant)
        evidence_available = tenant_evidence_available(tenant)
        if not evidence_available:
            issues.append("租户申请证据须为现存、非空、当前用户拥有的私密普通文件绝对路径")
        result = {"input_readiness": "blocked-input" if issues else "ready", "missing": issues,
                  "tenant_evidence_file": "available-content-not-reviewed" if evidence_available else "missing-or-invalid",
                  "identity_mapping": "approved-fix-under-validation", "real_login": "not-run"}
        review_issues = []
        if args.signoff_file:
            review = review_tenant_evidence(tenant, private_file(args.signoff_file))
            result["tenant_evidence_file"] = (
                "available-consistency-checked" if not review["issues"] else "available-review-blocked"
            )
            result["tenant_evidence_review"] = review
            review_issues = review["issues"]
        provider_failed = False
        if args.provider:
            try:
                result["provider"] = provider_summary()
            except (OSError, ValueError, KeyError):
                provider_failed = True
                result["provider"] = {
                    "status": "check-error",
                    "reason": "公开 discovery/JWKS 检查失败；未输出响应原文",
                    "real_login": "not-run", "provider_rotation": "not-run",
                }
        print(json.dumps(result, ensure_ascii=False, indent=2))
        if provider_failed:
            return 1
        return 2 if issues or review_issues else 0
    except (OSError, ValueError, KeyError):
        print(json.dumps({"input_readiness": "error", "reason": "输入、权限或 provider 检查失败；未输出原文"}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
