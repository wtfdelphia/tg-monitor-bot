-- 跨身份媒体中转的下载任务（docs/plan/proposals/WP-5-delivery-media.md 片 2，2026-10-20 批准）。
--
-- 一条 canonical event 一个 job：多个租户命中同一条消息只下载一次，所以主键就是
-- canonical_event_id，入队用 ON CONFLICT DO NOTHING。下载方是持有 receiver 身份租约的
-- mtproto-worker；delivery 经自己租户的 delivery_tasks → tenant_events 查到 job，等 READY 再发。
--
-- 丙类（spec/03 §1.3）：无 tenant_id 列，与 canonical_events 同属租户无关层，不启用 RLS。
-- 隔离靠应用层：job 只含 canonical_event_id 与下载状态，不含任何租户数据；
-- delivery 读 job 的唯一入口先在 tenant_tx 里经本租户的 tenant_events 找到 canonical_event_id。
--
-- 0012 的 ensure_rls event trigger 会给本表自动 ENABLE + FORCE RLS（它不分甲乙丙）。
-- 丙类表被误开 RLS 的后果是 app_user 静默读到 0 行（0012 末尾的注释），
-- 所以建表后显式关掉。两条 ALTER 本身幂等。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

CREATE TABLE IF NOT EXISTS media_relay_jobs (
  canonical_event_id   BIGINT PRIMARY KEY REFERENCES canonical_events(id) ON DELETE CASCADE,
  receiver_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  status               TEXT NOT NULL DEFAULT 'PENDING'
                       CHECK (status IN ('PENDING','DOWNLOADING','READY','DEAD')),
  media_archive_id     BIGINT REFERENCES media_archive(id),
  attempt_count        INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_retry_at        TIMESTAMPTZ,
  last_error_code      TEXT,
  created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- READY 必有档案，其余状态不得有：delivery 只凭 READY 取 object_key
  CONSTRAINT media_relay_jobs_ready_archive_check
    CHECK ((status = 'READY') = (media_archive_id IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_relay_due
  ON media_relay_jobs (receiver_identity_id, next_retry_at)
  WHERE status IN ('PENDING','DOWNLOADING');

ALTER TABLE media_relay_jobs NO FORCE ROW LEVEL SECURITY;
ALTER TABLE media_relay_jobs DISABLE ROW LEVEL SECURITY;

-- 不给 DELETE：job 随 canonical_events 级联清理（spec/04 的事件保留期），运行时不删。
GRANT SELECT, INSERT, UPDATE ON media_relay_jobs TO app_user;

RESET ROLE;
