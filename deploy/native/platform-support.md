# Native platform support boundary

This file is shipped with the native source archive. It is self-contained and does not link to development plans or acceptance logs.

## Supported acceptance targets

| Platform | Binary/material verification | Native service preflight | Current result | Allowed use |
|---|---|---|---|---|
| Rocky Linux 9.8 x86_64 | pass | pass | supported for non-production native acceptance | Production use still requires separate G5/WP-8 evidence. |
| CentOS Linux 7.9 x86_64 | pass | expected fail | binary compatible only; native service unsupported | Verify binary/signatures and fail-closed rejection only. Do not install or enable current native services. |
| Other Rocky/Alma/RHEL releases | not assessed | not assessed | unsupported until tested | Do not infer support from Rocky 9.8 results. |
| EL8 or other systemd 219/239 environments | not assessed | not assessed | unsupported until capability checks pass | Inspect unit directives and `+SECCOMP`; do not infer by version number. |

## Common minimum requirements

- Linux `x86_64`; candidate binaries target glibc ABI `2.17` or later.
- `cosign`, `tar`, `sha256sum`, `ldd`, `openssl` and `systemd-analyze` are available.
- Candidate signature, provenance, archive manifest and package checks all pass.
- systemd must not ignore unit security directives. Units using `RestrictAddressFamilies` require systemd built with `+SECCOMP`.
- Candidate installation is explicit and non-root, does not switch the stable link, and does not install or enable production services.

## CentOS 7 boundary

The tested CentOS 7.9 host reports systemd 219 with `-SECCOMP`. The address-family restriction required by current units cannot be proven effective, so preflight fails closed. Do not remove `RestrictAddressFamilies`, weaken sandboxing, replace PID 1 or report the expected rejection as a pass.

Any future CentOS 7 native-service support requires a separately approved compensating-security design and renewed unit, kernel-capability and acceptance review. Until then, CentOS 7 supports binary compatibility and rejection-path verification only.

## Release boundary

Stable releases, GHCR publication and production deployment require their own approved release process. Non-production platform acceptance is not production authorization.
