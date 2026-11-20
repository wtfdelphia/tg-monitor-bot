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
-- 偏离 spec/03 §5.2 与 eng/04 §二 A4 的白名单：该处把本索引列为「不含 tenant_id
-- 是有意的，idem_key 本身是含 tenant 的五元组哈希」。实测下来这个论证不成立。
--
-- 哈希把 tenant 藏进了值里，但唯一约束泄漏的是**存在性**，与值是否可读无关。
-- 攻击者不需要读出 idem_key，只需要能构造候选：
--   idem_key = sha256("tenant|rule_id|rule_version|event_id|target")
--   target 是公开频道 id —— 自己订阅同一频道即得知，不是秘密；
--   tenant / rule_id / rule_version / event_id 全是小整数，枚举空间很小。
-- 实测（攻击者 tenant 5，受害 tenant 6 有一条 rule 7 / v3 / 事件 42 的任务）：
--   ON CONFLICT (idem_key) DO NOTHING 批量插入 5000 个候选，一条语句 0.58 秒，
--   返回「插入成功 4999 行」—— 差值 1 就是 oracle，指出哪个五元组真实存在。
--   不报错、不留行（回滚即可）、不逐个往返，所以速率限制也拦不住。
--
-- 故加上 tenant_id。同一批探测改后返回 5000（无信息），
-- 而同租户内重复 idem_key 仍报 duplicate key —— 幂等这项正业没有削弱：
-- tenant_id 本来就在哈希输入里，把它加进索引键不改变任何一对 (tenant, key)
-- 的唯一性判定，只是不再把不同租户的键放进同一个命名空间比较。
-- 相应地本索引已从 eng/04 A4 的白名单里移除，由 A4 正常覆盖。
--
-- 与另两项白名单的差别在于**它没有 GRANT 补偿可用**：
-- bot_console_sessions / web_sessions 能对 app_user 零权限，
-- delivery_tasks 是业务主表，收不回来。所以只能改约束形状。
CREATE UNIQUE INDEX IF NOT EXISTS uq_task_idem
  ON delivery_tasks (tenant_id, idem_key) WHERE idem_key IS NOT NULL;

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
