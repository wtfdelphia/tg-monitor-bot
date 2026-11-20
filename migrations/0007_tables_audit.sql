-- 审计与运维域。DDL 抄自 docs/design/spec/03-数据模型.md §七 及 §7.1~§7.4。
-- 本文件不含 REVOKE/GRANT：spec/03 把它们与 DDL 写在一起是最终态视图，
-- 而权限必须晚于 RLS 启用（迁移顺序见 eng/02 §三），统一落在 0011_grants.sql。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

CREATE TABLE IF NOT EXISTS audit_logs (
  id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id   BIGINT REFERENCES tenants(id) ON DELETE CASCADE,
  actor_user_id BIGINT,                   -- who
  action      TEXT NOT NULL,              -- 'rule.create' / 'identity.delete'
  resource    TEXT NOT NULL,
  resource_id BIGINT,
  before      JSONB,
  after       JSONB,
  -- 入口来源。Bot 入口没有 IP，这是它唯一的可追溯来源标识
  source      TEXT NOT NULL DEFAULT 'web' CHECK (source IN ('web','bot','system')),
  ip          INET,                       -- web 有；bot 为 NULL
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_audit_tenant ON audit_logs (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_logs (action, created_at DESC);

-- 账号租约与防脑裂，详见 docs/design/spec/08-账号接入.md
CREATE TABLE IF NOT EXISTS account_leases (
  identity_id   BIGINT PRIMARY KEY REFERENCES identities(id) ON DELETE CASCADE,
  worker_id     TEXT NOT NULL,
  fencing_token BIGINT NOT NULL,
  heartbeat_at  TIMESTAMPTZ NOT NULL,
  expires_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lease_expired ON account_leases (expires_at);

CREATE TABLE IF NOT EXISTS login_sessions (
  id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id   BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('qr','code')),
  state       TEXT NOT NULL CHECK (state IN
              ('pending','awaiting_password','done','expired','failed')),
  -- QR：exportLoginToken 返回的 token。这是 bearer 凭据
  login_token BYTEA,
  dc_id       INTEGER,                    -- DC 迁移后的目标 DC
  expires_at  TIMESTAMPTZ NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_login_expire ON login_sessions (expires_at);

CREATE TABLE IF NOT EXISTS system_logs (
  id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id  BIGINT REFERENCES tenants(id) ON DELETE CASCADE,
  level      TEXT NOT NULL,
  module     TEXT NOT NULL,
  event      TEXT NOT NULL,
  context    JSONB,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_syslog_time ON system_logs (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_syslog_module ON system_logs (module, created_at DESC);

-- §7.1 控制 Bot 入口两表。只服务控制 Bot，与采集/投递链路无关
-- Bot 私聊的服务端会话上下文。一个 tg_user_id 可属多个租户，
-- 当前选中哪个必须存在服务端 —— 放 callback_data 里就是可被篡改的越权入口
CREATE TABLE IF NOT EXISTS bot_console_sessions (
  tg_user_id BIGINT PRIMARY KEY,         -- 鉴权入口，故不以 tenant_id 为主键
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  state      JSONB,                      -- 多步命令的中间态
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  FOREIGN KEY (tenant_id, tg_user_id) REFERENCES tenant_members(tenant_id, user_id)
    ON DELETE CASCADE                    -- 成员关系被撤销时上下文自动失效
);

-- 控制命令的幂等承载
CREATE TABLE IF NOT EXISTS bot_console_updates (
  tenant_id   BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  -- Bot API 的 update_id 按 Bot 独立计数，两个 Bot 出现同号是正常的，
  -- 故 bot_id 必须在键里（实测 (1,51,9001) 与 (1,52,9001) 均插入成功）
  bot_id      BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  update_id   BIGINT NOT NULL,
  processed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, bot_id, update_id)
);
CREATE INDEX IF NOT EXISTS idx_console_updates_time
  ON bot_console_updates (processed_at);

-- §7.2 控制台扫码登录的挑战。租户未知，故本表无 tenant_id（丙类，不启用 RLS）
CREATE TABLE IF NOT EXISTS console_login_challenges (
  nonce        BYTEA PRIMARY KEY,           -- 随机 32 字节，进二维码的就是它
  -- 确认码。网页与 Bot 确认卡片同时展示，用户目视比对以防钓鱼
  confirm_code CHAR(4) NOT NULL,
  state        TEXT NOT NULL DEFAULT 'pending' CHECK (state IN
               ('pending','awaiting_confirm','confirmed','rejected','expired')),
  -- 应答者。awaiting_confirm 起有值；确认后据此查 tenant_members
  tg_user_id   BIGINT,
  -- 发起端指纹。确认卡片必须展示这两项，否则用户无从判断是否是自己发起的
  origin_ip    INET NOT NULL,
  user_agent   TEXT,
  expires_at   TIMESTAMPTZ NOT NULL,        -- 2 分钟
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_console_login_expire
  ON console_login_challenges (expires_at);
-- 一个 tg_user_id 同时只允许一个待确认挑战，防「连发多张卡片诱导误点」
CREATE UNIQUE INDEX IF NOT EXISTS uq_console_login_pending
  ON console_login_challenges (tg_user_id) WHERE state = 'awaiting_confirm';

-- §7.3 REST/Web 入口的服务端会话。有状态而非 JWT：
-- 成员被移出 tenant_members 后会话须立即失效，JWT 做不到
CREATE TABLE IF NOT EXISTS web_sessions (
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  id         BYTEA NOT NULL,               -- 随机 32 字节，非自增（自增可枚举）
  user_id    BIGINT NOT NULL,
  csrf_token BYTEA NOT NULL,
  ip         INET,
  user_agent TEXT,
  expires_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES tenant_members(tenant_id, user_id)
    ON DELETE CASCADE                      -- 成员关系撤销即踢下线
);
-- A4 白名单之一：全局唯一性由它保证，故主键得以带上 tenant_id 不破铁律 1
CREATE UNIQUE INDEX IF NOT EXISTS uq_web_session_id ON web_sessions (id);
CREATE INDEX IF NOT EXISTS idx_web_sessions_user ON web_sessions (tenant_id, user_id);

-- §7.4 租户授权凭证。与 audit_logs 的区别：这里记「同意了什么」，不是「做了什么」
CREATE TABLE IF NOT EXISTS tenant_consents (
  id           BIGINT GENERATED ALWAYS AS IDENTITY,
  tenant_id    BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  -- free 档只需 tos（free 不代持凭据，无需代持授权）
  consent_type TEXT NOT NULL
               CHECK (consent_type IN ('tos','privacy','credential_custody')),
  -- 授权文本版本。文本改了而旧租户没重新同意，旧授权就不覆盖新行为
  text_version TEXT NOT NULL,
  -- 同意者。落在具体自然人身上，不是「租户」这个抽象
  user_id      BIGINT NOT NULL,
  agreed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  ip           INET,
  user_agent   TEXT,
  PRIMARY KEY (tenant_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES tenant_members(tenant_id, user_id)
    ON DELETE CASCADE
);
-- 查询形态：「租户 X 对 Y 类授权的最新同意是什么」
-- §7.4 明确两条索引刻意不加：UNIQUE (tenant_id, id) 主键已是它；
-- UNIQUE (tenant_id, consent_type, text_version, user_id) 会让「同意→撤回→再同意」写不进去
CREATE INDEX IF NOT EXISTS idx_tenant_consents_latest
  ON tenant_consents (tenant_id, consent_type, agreed_at DESC);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
