-- Bot 的 MTProto session 加密落库，重启复用，不再每次 bot_sign_in（ADR-0034）。
-- 与 Token 同行、同一个 DEK 加密，AAD 前缀不同（tgm_core::credential）；两列要么都有要么都空。
-- Token 被覆盖时由写入语句一并清空（crates/db/src/credentials.rs 的 UPSERT_SECRET），
-- 所以旧 Token 的 session 不会搭上新 DEK 留下来。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

ALTER TABLE identity_secrets
  ADD COLUMN IF NOT EXISTS bot_session_ciphertext BYTEA;
ALTER TABLE identity_secrets
  ADD COLUMN IF NOT EXISTS bot_session_nonce BYTEA;

ALTER TABLE identity_secrets
  DROP CONSTRAINT IF EXISTS identity_secrets_bot_session_pair_check;
ALTER TABLE identity_secrets
  ADD CONSTRAINT identity_secrets_bot_session_pair_check
  CHECK ((bot_session_ciphertext IS NULL) = (bot_session_nonce IS NULL));

RESET ROLE;
