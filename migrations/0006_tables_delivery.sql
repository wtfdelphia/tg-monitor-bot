-- 投递域。DDL 抄自 docs/design/spec/03-数据模型.md §六。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

CREATE TABLE IF NOT EXISTS delivery_tasks (
  id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id       BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  tenant_event_id BIGINT,
  rule_id         BIGINT,                 -- 快照语义，规则删除后不置空，故不设 FK
  rule_version    INTEGER,                -- 配 rule_id 定位 rule_versions 快照
  -- 快照：Telegram 原生值，不随配置变更
  target_tg_chat_id  BIGINT NOT NULL,
  -- identities 是铁律 4 的唯一例外（见 §1.2），保持单列
  sender_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE RESTRICT,
  action          TEXT NOT NULL CHECK (action IN ('COPY','FORWARD','RESEND')),
  -- CANCELLED：租户转 suspended 时已入队任务的终态（spec/01 §七），不重试、不进死信
  status          TEXT NOT NULL DEFAULT 'PENDING'
                  CHECK (status IN ('PENDING','SENDING','DONE','FAILED','DEAD','CANCELLED')),
  attempt_count   INTEGER NOT NULL DEFAULT 0,
  next_retry_at   TIMESTAMPTZ,
  last_error_code TEXT,
  -- 投递成功后回填。「编辑目标消息」能力依赖此字段
  telegram_message_id BIGINT,
  idem_key        TEXT,                   -- 内部生成，与外部 Idempotency-Key 分离
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  completed_at    TIMESTAMPTZ,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  FOREIGN KEY (tenant_id, tenant_event_id)
    REFERENCES tenant_events (tenant_id, id) ON DELETE CASCADE
);
-- 调度器扫描路径
CREATE INDEX IF NOT EXISTS idx_task_due ON delivery_tasks (next_retry_at)
  WHERE status IN ('PENDING','FAILED');
CREATE INDEX IF NOT EXISTS idx_task_tenant ON delivery_tasks (tenant_id, created_at DESC);
-- A4 白名单之一：不含 tenant_id 是有意的，idem_key 本身是含 tenant 的五元组哈希
CREATE UNIQUE INDEX IF NOT EXISTS uq_task_idem
  ON delivery_tasks (idem_key) WHERE idem_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS delivery_logs (
  id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  task_id   BIGINT NOT NULL REFERENCES delivery_tasks(id) ON DELETE CASCADE,
  attempt   INTEGER NOT NULL,
  ok        BOOLEAN NOT NULL,
  error_code TEXT,
  error_msg TEXT,
  latency_ms INTEGER,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_dlog_task ON delivery_logs (task_id);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
