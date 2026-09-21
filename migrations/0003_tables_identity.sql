-- 身份与凭据域。DDL 抄自 docs/design/spec/03-数据模型.md §三。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

-- 统一的「接收/发送身份」抽象，Bot 与 MTProto 账号共用
CREATE TABLE IF NOT EXISTS identities (
  id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  kind         TEXT NOT NULL CHECK (kind IN ('bot','mtproto')),
  -- 平台演示身份的 tenant_id 为 NULL（全局共享，仅供演示源使用）
  -- 这是铁律 5 的三处有意破例之一，也是铁律 4 唯一例外的成因
  tenant_id    BIGINT REFERENCES tenants(id) ON DELETE CASCADE,
  tg_user_id   BIGINT,                    -- 该身份自身的 Telegram user id
  display_name TEXT,
  status       TEXT NOT NULL DEFAULT 'offline'
               CHECK (status IN ('online','offline','flood_wait','banned','unauthorized')),
  proxy_url    TEXT,                      -- 按身份独立代理，SOCKS5/HTTP
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_identities_tenant
  ON identities (tenant_id) WHERE tenant_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_identities_shared
  ON identities (kind) WHERE tenant_id IS NULL;

-- 每租户一个 DEK，密文由 KMS 中的 KEK 加密
CREATE TABLE IF NOT EXISTS credential_keys (
  tenant_id     BIGINT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
  dek_ciphertext BYTEA NOT NULL,          -- 被 KEK 加密后的 DEK
  kek_id        TEXT NOT NULL,            -- KMS 中的 KEK 标识
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  rotated_at    TIMESTAMPTZ
);

-- 凭据密文。AES-256-GCM，密钥为该租户 DEK
CREATE TABLE IF NOT EXISTS identity_secrets (
  identity_id      BIGINT PRIMARY KEY REFERENCES identities(id) ON DELETE CASCADE,
  -- Bot：token 密文；MTProto：session 密文
  secret_ciphertext BYTEA NOT NULL,
  nonce            BYTEA NOT NULL,
  api_id           INTEGER,               -- MTProto 专用，租户自带（ADR-0013）
  api_hash_ciphertext BYTEA,              -- MTProto 专用
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
