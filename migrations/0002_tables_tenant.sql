-- 租户域。DDL 抄自 docs/design/spec/03-数据模型.md §二，那里是唯一定义处。
--
-- 关于 0002 的切分（pre-do/01-待建清单.md §2.2 的待决项）：
--   eng/02 §三 把 27 张表放进一个 0002_tables.sql，而 ADR-0021 要求
--   「每个迁移文件尽量只做一件事」，理由是 sqlx 按文件记录版本，
--   一个文件里多条语句时部分成功的状态是真实存在的。
--   切分依据是外键依赖的拓扑序 —— 实测该序与 spec/03 的六个域边界完全重合，
--   没有一条跨域反向引用，所以按域切成 0002a~0002f 即是一个合法拓扑序。
--
-- 为什么每个 DDL 文件都以 SET ROLE 开头（eng/02 §三 没写这一条）：
--   表的 owner 是建表者。迁移以 postgres（超级用户）连库，不换角色则 27 张表
--   全归 postgres —— 而超级用户不受 RLS 约束，FORCE 也拦不住（spec/05 §4.2 的表）。
--   实测同一套 ENABLE + FORCE + TO app_user 策略下：
--     owner = postgres   → owner 自己 SELECT 看见 2 行   ← FORCE 形同虚设
--     owner = tgm_owner  → owner 自己 SELECT 看见 0 行，INSERT 被策略拒
--   所以 SET ROLE 不是风格，它决定 FORCE 这一层是真的还是装饰。
--   A2 断言「运行时角色不得是任何表的 owner」查的是 app_user，查不出这一种失效。
SET ROLE tgm_owner;

CREATE TABLE IF NOT EXISTS tenants (
  id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name          TEXT NOT NULL,
  -- 两档。enterprise 不做，见 ADR-0016
  plan          TEXT NOT NULL DEFAULT 'free'
                CHECK (plan IN ('free','pro')),
  owner_user_id BIGINT NOT NULL,          -- Telegram user id
  status        TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active','suspended','deleted')),
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_tenants_owner ON tenants (owner_user_id);

-- 首发：建表但硬编码单成员（API 层固定 user_id = tenants.owner_user_id）
-- 后续迭代无需改表
CREATE TABLE IF NOT EXISTS tenant_members (
  tenant_id  BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id    BIGINT NOT NULL,            -- Telegram user id，Bot 入口按此反查
  -- 五级角色里的四级。SUPER_ADMIN 是平台级角色，不属于任何租户，故不在此表
  role       TEXT NOT NULL DEFAULT 'TENANT_OWNER'
             CHECK (role IN ('TENANT_OWNER','TENANT_ADMIN','MEMBER','VIEWER')),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, user_id)
);
-- Bot 入口的鉴权反查路径：按 user_id 找「他属于哪些租户」
-- 主键是 (tenant_id, user_id)，缺此索引反查会走全表
CREATE INDEX IF NOT EXISTS idx_members_user ON tenant_members (user_id);

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
