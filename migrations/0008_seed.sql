-- 种子数据。位置由 docs/design/eng/02-本地环境与迁移.md §三 固定：
-- 必须在 0009（ENABLE + FORCE）之前。理由是坑 2 ——
-- FORCE 之后 owner 自己也受策略约束，而没有任何策略匹配 owner，默认拒绝。
-- 实测报错是 `new row violates row-level security policy`，
-- 执行者是 owner、策略写的是 TO app_user，照样被拦。
--
-- 本文件当前没有 INSERT。这不是漏写，是设计文档里确实没有定义任何种子行：
--   · 全套 docs/design 下只有三处 INSERT，全是实测记录或样例，不是种子数据
--   · 唯一的平台级数据候选是「演示身份」（identities.tenant_id IS NULL，
--     spec/02 §4.1），但它需要一个平台真实拥有的频道 + 一份 Bot token。
--     token 是凭据，不能进仓库；频道也还没建。所以它不该由迁移创建，
--     应由 tgm 的一条运维命令在部署后灌入。
--
-- 文件保留而不删除，是为了占住这个序号位置 ——
-- 一旦将来有种子行要灌，它必须落在这里而不是 0009 之后。
-- 届时写法用 ON CONFLICT DO NOTHING（ADR-0021：迁移必须可重入）。
--
-- 建表与本文件都以 tgm_owner 身份执行，理由见 0002 的同一行注释。
SET ROLE tgm_owner;

-- 无操作。留一条空语句让文件不是纯注释（sqlx 会把文件内容整段送给 PG）。
SELECT 1;

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
