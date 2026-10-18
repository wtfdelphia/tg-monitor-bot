-- 每身份一个 DEK（ADR-0032）。DEK 密文与 kek_id 跟凭据密文同行存放，
-- control-plane 写入只需 kms:Encrypt，不再需要解开租户共享 DEK。
-- credential_keys 停止写入但保留：已应用的迁移不改（ADR-0021），旧行由删除路径清理。
--
-- 两列直接 NOT NULL、不给默认值：表里已有行时本迁移会失败。#12 之后没有任何部署
-- 写入过凭据（生产未上线，本地只有测试临时库），所以失败只会说明这条假设不成立 ——
-- 那时应当停下来人工处理，而不是让一批没有 DEK 的旧行静默留在表里。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

ALTER TABLE identity_secrets
  ADD COLUMN IF NOT EXISTS dek_ciphertext BYTEA NOT NULL;
ALTER TABLE identity_secrets
  ADD COLUMN IF NOT EXISTS kek_id TEXT NOT NULL;

RESET ROLE;
