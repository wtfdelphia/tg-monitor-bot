SET ROLE tgm_owner;

ALTER TABLE identities
  ADD COLUMN IF NOT EXISTS flood_wait_until TIMESTAMPTZ NOT NULL DEFAULT '-infinity';

GRANT UPDATE (flood_wait_until) ON identities TO app_user;

RESET ROLE;
