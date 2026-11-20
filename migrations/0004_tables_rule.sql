-- 订阅与规则域。DDL 抄自 docs/design/spec/03-数据模型.md §四 与 §4.1。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

-- 「哪个租户、监控哪个频道、由哪个身份接收」—— 双模式共存的关键
CREATE TABLE IF NOT EXISTS source_subscriptions (
  id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id            BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  tg_chat_id           BIGINT NOT NULL,           -- Telegram 原生值
  title                TEXT,
  -- 公开频道的 @username，不含 @。私有群为 NULL（正常态，非异常）。
  -- 缓存列，随权限探测刷新；ADR-0023。不得进任何唯一约束 ——
  -- 多个租户会订阅同一个公开频道。
  username             TEXT,
  receiver_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE RESTRICT,
  -- 授权准入：私有群/频道须验证操作者为 Owner/Admin，来源是 my_chat_member
  authorized_by        BIGINT,                    -- 授权操作人 Telegram user id
  authorized_at        TIMESTAMPTZ,
  enabled              BOOLEAN NOT NULL DEFAULT TRUE,
  discovered           BOOLEAN NOT NULL DEFAULT FALSE,  -- dialogs 自动发现
  created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_subscription
  ON source_subscriptions (tenant_id, tg_chat_id, receiver_identity_id);
-- 承铁律 4：供子表（rules / tenant_events）挂复合外键
CREATE UNIQUE INDEX IF NOT EXISTS uq_source_subscriptions_tenant_id
  ON source_subscriptions (tenant_id, id);
-- 扇出层的核心查询路径
CREATE INDEX IF NOT EXISTS idx_subscription_fanout
  ON source_subscriptions (tg_chat_id, receiver_identity_id) WHERE enabled;

CREATE TABLE IF NOT EXISTS target_channels (
  id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id   BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  tg_chat_id  BIGINT NOT NULL,
  title       TEXT,
  sender_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE RESTRICT,
  can_send    BOOLEAN,                    -- 连通性自检结果（ADR-0019）
  checked_at  TIMESTAMPTZ,
  enabled     BOOLEAN NOT NULL DEFAULT TRUE,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_target ON target_channels (tenant_id, tg_chat_id);
-- 承铁律 4：供 rules.target_ref 挂复合外键
CREATE UNIQUE INDEX IF NOT EXISTS uq_target_channels_tenant_id
  ON target_channels (tenant_id, id);

CREATE TABLE IF NOT EXISTS keywords (
  id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  word       TEXT NOT NULL,
  match_type TEXT NOT NULL DEFAULT 'exact'
             CHECK (match_type IN ('exact','regex','fuzzy')),
  category   TEXT,
  priority   INTEGER NOT NULL DEFAULT 0,
  enabled    BOOLEAN NOT NULL DEFAULT TRUE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_keyword ON keywords (tenant_id, word, match_type);
-- 承铁律 4：供 rules.keyword_id 挂复合外键
CREATE UNIQUE INDEX IF NOT EXISTS uq_keywords_tenant_id ON keywords (tenant_id, id);
-- 热加载：按 updated_at 增量比对
CREATE INDEX IF NOT EXISTS idx_keyword_reload
  ON keywords (tenant_id, updated_at) WHERE enabled;

CREATE TABLE IF NOT EXISTS rules (
  id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  keyword_id BIGINT NOT NULL,
  -- NULL = 该租户全部来源
  source_ref BIGINT,
  media_type TEXT CHECK (media_type IN
             ('ANY','TEXT','PHOTO','VIDEO','AUDIO','VOICE','VIDEO_NOTE',
              'DOCUMENT','STICKER','ANIMATION','OTHER')),
  target_ref BIGINT NOT NULL,
  action     TEXT NOT NULL DEFAULT 'COPY'
             CHECK (action IN ('COPY','FORWARD','RESEND')),
  priority   INTEGER NOT NULL DEFAULT 0,
  -- 命中即停，用于黑名单拦截
  stop_processing BOOLEAN NOT NULL DEFAULT FALSE,
  template   TEXT,                        -- 来源头模板，NULL = 不加头
  -- 规则版本。每次语义变更 +1，使历史投递可解释
  version    INTEGER NOT NULL DEFAULT 1,
  enabled    BOOLEAN NOT NULL DEFAULT TRUE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- 承铁律 4：三条引用全部走复合键，跨租户边在库层不可表达
  FOREIGN KEY (tenant_id, keyword_id)
    REFERENCES keywords (tenant_id, id) ON DELETE RESTRICT,
  FOREIGN KEY (tenant_id, source_ref)
    REFERENCES source_subscriptions (tenant_id, id) ON DELETE RESTRICT,
  FOREIGN KEY (tenant_id, target_ref)
    REFERENCES target_channels (tenant_id, id) ON DELETE RESTRICT
);
-- 承铁律 4：供子表（rule_versions / rule_conditions / rule_replacements）挂复合外键
CREATE UNIQUE INDEX IF NOT EXISTS uq_rules_tenant_id ON rules (tenant_id, id);
-- 冲突检查（API 统一返回 409）
CREATE UNIQUE INDEX IF NOT EXISTS uq_rule_conflict ON rules (
  tenant_id, keyword_id,
  COALESCE(source_ref, 0),
  COALESCE(media_type, 'ANY'),
  target_ref
) WHERE enabled;

-- 规则版本历史。只存快照，供投递记录反查「当时的规则长什么样」
CREATE TABLE IF NOT EXISTS rule_versions (
  rule_id    BIGINT NOT NULL,
  version    INTEGER NOT NULL,
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  snapshot   JSONB NOT NULL,              -- 规则 + 条件 + 替换的完整快照
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- 偏离 spec/03 §4.2：该处写的是 PRIMARY KEY (rule_id, version)，缺 tenant_id。
  -- 那样 A4 会报红，而且不是误报 —— 实测（攻击者 tenant 1，受害 rule_id 1 属 tenant 2）：
  --   (1, 1,   tenant_id=1) → duplicate key ... "rule_versions_pkey"
  --   (1, 999, tenant_id=1) → FK violation "Key is not present in table rules"
  -- 两种报错可分，就是一个可枚举的 oracle：能探出他租户有哪些规则、各改了几版。
  -- 下面那道复合外键拦不住它 —— 唯一索引在堆插入期检查，外键是 AFTER 触发器，
  -- 唯一约束永远先报。加 tenant_id 不削弱唯一性：复合外键已把 tenant_id 钉死为
  -- rule_id 对应的唯一值，两种 PK 的唯一性等价。同胞表 rule_replacements 同形，
  -- 它的 PK 本来就是 (tenant_id, rule_id, seq) —— 故这里是 spec 漏列，不是破例。
  PRIMARY KEY (tenant_id, rule_id, version),
  FOREIGN KEY (tenant_id, rule_id) REFERENCES rules (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_rule_versions_tenant
  ON rule_versions (tenant_id, created_at DESC);

CREATE TABLE IF NOT EXISTS rule_conditions (
  id       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  rule_id  BIGINT NOT NULL REFERENCES rules(id) ON DELETE CASCADE,
  kind     TEXT NOT NULL CHECK (kind IN
           ('exclude_word','file_name','size_min','size_max','sender',
            'ext_allow','ext_deny')),
  value    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rule_cond ON rule_conditions (rule_id);

-- 正文改写。同类项目的高频需求
CREATE TABLE IF NOT EXISTS rule_replacements (
  rule_id     BIGINT NOT NULL,
  seq         SMALLINT NOT NULL,           -- 执行序，显式声明不靠 id 隐式定序
  tenant_id   BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  pattern     TEXT NOT NULL,               -- Rust regex 语法
  replacement TEXT NOT NULL DEFAULT '',    -- 空串 = 删除匹配内容
  enabled     BOOLEAN NOT NULL DEFAULT TRUE,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, rule_id, seq),
  FOREIGN KEY (tenant_id, rule_id) REFERENCES rules (tenant_id, id) ON DELETE CASCADE
);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
