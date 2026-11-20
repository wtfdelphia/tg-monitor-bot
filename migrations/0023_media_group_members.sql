SET ROLE tgm_owner;

CREATE TABLE IF NOT EXISTS media_group_members (
  group_event_id BIGINT NOT NULL REFERENCES canonical_events(id) ON DELETE CASCADE,
  member_event_id BIGINT NOT NULL REFERENCES canonical_events(id) ON DELETE CASCADE,
  position INTEGER NOT NULL CHECK (position >= 0),
  PRIMARY KEY (group_event_id, member_event_id),
  UNIQUE (member_event_id),
  UNIQUE (group_event_id, position),
  CHECK (group_event_id <> member_event_id)
);

ALTER TABLE media_group_members NO FORCE ROW LEVEL SECURITY;
ALTER TABLE media_group_members DISABLE ROW LEVEL SECURITY;

GRANT SELECT, INSERT ON media_group_members TO app_user;

RESET ROLE;
