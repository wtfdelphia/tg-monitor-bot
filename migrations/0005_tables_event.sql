-- 事件域（两层）。DDL 抄自 docs/design/spec/03-数据模型.md §五 与 §5.3。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

-- 第一层：规范化原始事件。与租户无关，可被多租户共享
CREATE TABLE IF NOT EXISTS canonical_events (
  id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  adapter              TEXT NOT NULL CHECK (adapter IN ('bot','mtproto')),
  receiver_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  tg_chat_id           BIGINT NOT NULL,
  tg_message_id        BIGINT NOT NULL,
  event_type           TEXT NOT NULL DEFAULT 'MESSAGE_CREATED'
                       CHECK (event_type IN (
                         'MESSAGE_CREATED','MESSAGE_EDITED','MESSAGE_DELETED',
                         'MEDIA_GROUP_COMPLETED','SOURCE_PERMISSION_CHANGED')),
  -- 编辑版本。同一 message_id 被多次编辑时区分，0 = 原始消息
  edit_version         INTEGER NOT NULL DEFAULT 0,
  -- Bot API 专用幂等键。能挡住 Telegram 重投 Webhook（tg_message_id 挡不住）
  update_id            BIGINT,
  -- 媒体组聚合键。同组多条消息共享
  media_group_id       TEXT,
  sender_id            BIGINT,
  message_date         TIMESTAMPTZ NOT NULL,
  text                 TEXT,
  caption              TEXT,
  media_type           TEXT,
  content_hash         TEXT,              -- L2 去重主键
  file_unique_id       TEXT,              -- 辅助，可空。跨适配器取值不一致
  file_name            TEXT,
  file_size            BIGINT,
  mime_type            TEXT,
  raw_file_id          TEXT,              -- 绑定 receiver_identity，跨身份不可用
  fanned_out           BOOLEAN NOT NULL DEFAULT FALSE,
  matched_any          BOOLEAN NOT NULL DEFAULT FALSE,  -- 决定保留期
  created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- L1 去重兜底。含 event_type + edit_version，使编辑事件不被误判为重复
CREATE UNIQUE INDEX IF NOT EXISTS uq_canonical ON canonical_events
  (receiver_identity_id, tg_chat_id, tg_message_id, event_type, edit_version);
-- Bot 侧额外幂等层：Telegram 重投同一 update 时直接撞唯一约束
CREATE UNIQUE INDEX IF NOT EXISTS uq_canonical_update
  ON canonical_events (receiver_identity_id, update_id)
  WHERE update_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_canonical_pending
  ON canonical_events (created_at) WHERE NOT fanned_out;
CREATE INDEX IF NOT EXISTS idx_canonical_mgroup
  ON canonical_events (receiver_identity_id, media_group_id)
  WHERE media_group_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_canonical_cleanup
  ON canonical_events (created_at, matched_any);

-- 第二层：租户级事件。一租户一条
CREATE TABLE IF NOT EXISTS tenant_events (
  id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id          BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  -- canonical_events 是丙类（无 tenant_id），铁律 4 不适用，保持单列
  canonical_event_id BIGINT NOT NULL REFERENCES canonical_events(id) ON DELETE CASCADE,
  source_ref         BIGINT NOT NULL,
  matched_keywords   TEXT[],
  matched            BOOLEAN NOT NULL DEFAULT FALSE,
  processed_at       TIMESTAMPTZ,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
  FOREIGN KEY (tenant_id, source_ref)
    REFERENCES source_subscriptions (tenant_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_tenant_event
  ON tenant_events (tenant_id, canonical_event_id);
-- 承铁律 4：供 delivery_tasks.tenant_event_id 挂复合外键
CREATE UNIQUE INDEX IF NOT EXISTS uq_tenant_events_tenant_id
  ON tenant_events (tenant_id, id);
-- 清理路径的反查（uq_tenant_event 以 tenant_id 为首列，覆盖不了）
CREATE INDEX IF NOT EXISTS idx_tenant_event_canonical
  ON tenant_events (canonical_event_id);
CREATE INDEX IF NOT EXISTS idx_tenant_event_pending
  ON tenant_events (tenant_id, created_at) WHERE processed_at IS NULL;

-- 媒体档案。L2 去重的长期层
CREATE TABLE IF NOT EXISTS media_archive (
  id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  content_hash   TEXT NOT NULL,           -- 主去重键
  file_unique_id TEXT,                    -- 辅助，可空可重复
  media_type     TEXT NOT NULL,
  mime_type      TEXT,
  file_name      TEXT,
  file_size      BIGINT,
  -- 跨身份中转：对象存储中的键
  object_key     TEXT,
  object_expires_at TIMESTAMPTZ,
  first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_media_hash ON media_archive (content_hash);
CREATE INDEX IF NOT EXISTS idx_media_expire ON media_archive (object_expires_at)
  WHERE object_key IS NOT NULL;

-- L3 通知级去重（§5.3）。同一资源被多群复读时抑制
CREATE TABLE IF NOT EXISTS notify_dedup (
  tenant_id   BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  fingerprint TEXT NOT NULL,              -- sha256(sender|hits|text[:300])
  target_ref  BIGINT NOT NULL,
  last_sent_at TIMESTAMPTZ NOT NULL,
  expires_at  TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (tenant_id, fingerprint, target_ref)
);
CREATE INDEX IF NOT EXISTS idx_notify_expire ON notify_dedup (expires_at);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
