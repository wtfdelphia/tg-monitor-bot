#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time
import unittest
import uuid


ROOT = Path(__file__).resolve().parent.parent
DOCKER = ["docker", "--host", "unix:///var/run/docker.sock"]
TABLES = {
    "account_leases", "audit_logs", "bot_console_sessions", "bot_console_updates",
    "canonical_events", "console_login_challenges", "credential_keys", "delivery_logs",
    "delivery_tasks", "identities", "identity_secrets", "keywords", "login_sessions",
    "media_archive", "media_group_members", "media_relay_jobs", "notify_dedup",
    "rule_conditions", "rule_replacements", "rule_versions", "rules",
    "source_subscriptions", "system_logs", "target_channels", "tenant_consents",
    "tenant_events", "tenant_members", "tenant_notifications", "tenants", "web_sessions",
}
TENANT_CHILDREN = {
    "audit_logs", "bot_console_sessions", "bot_console_updates", "credential_keys",
    "delivery_tasks", "identities", "keywords", "login_sessions", "notify_dedup",
    "rule_replacements", "rule_versions", "rules", "source_subscriptions", "system_logs",
    "target_channels", "tenant_consents", "tenant_events", "tenant_members",
    "tenant_notifications", "web_sessions",
}
IDENTITY_CHILDREN = {
    "account_leases": ("identity_id", "c"),
    "bot_console_updates": ("bot_id", "c"),
    "canonical_events": ("receiver_identity_id", "c"),
    "delivery_tasks": ("sender_identity_id", "r"),
    "identity_secrets": ("identity_id", "c"),
    "media_relay_jobs": ("receiver_identity_id", "c"),
    "source_subscriptions": ("receiver_identity_id", "r"),
    "target_channels": ("sender_identity_id", "r"),
    "tenant_notifications": ("identity_id", "c"),
}
ROOT_AUTHORIZATION = {
    "owner": "tgm_owner", "row_security": False, "force_row_security": False,
    "delete_privileges": {
        "app_user": False, "tenant_lifecycle": False, "tenant_cleanup": False,
        "shared_cleanup": False, "tgm_owner": True,
    },
}
OTHER_EDGES = [
    ("media_group_members", "group_event_id", "canonical_events", "id", "c", ()),
    ("media_group_members", "member_event_id", "canonical_events", "id", "a", ()),
    ("media_relay_jobs", "canonical_event_id", "canonical_events", "id", "c", ()),
    ("tenant_events", "canonical_event_id", "canonical_events", "id", "a", ()),
    ("delivery_logs", "task_id", "delivery_tasks", "id", "r", ()),
    ("media_relay_jobs", "media_archive_id", "media_archive", "id", "a", ()),
    ("rules", "tenant_id,keyword_id", "keywords", "tenant_id,id", "r", ()),
    ("rules", "tenant_id,source_ref", "source_subscriptions", "tenant_id,id", "r", ()),
    ("rules", "tenant_id,target_ref", "target_channels", "tenant_id,id", "r", ()),
    ("rule_conditions", "rule_id", "rules", "id", "c", ()),
    ("rule_replacements", "tenant_id,rule_id", "rules", "tenant_id,id", "c", ()),
    ("rule_versions", "tenant_id,rule_id", "rules", "tenant_id,id", "c", ()),
    ("tenant_events", "tenant_id,source_ref", "source_subscriptions", "tenant_id,id", "c", ()),
    ("delivery_tasks", "tenant_id,tenant_event_id", "tenant_events", "tenant_id,id", "n", ("tenant_event_id",)),
    ("bot_console_sessions", "tenant_id,tg_user_id", "tenant_members", "tenant_id,user_id", "c", ()),
    ("tenant_consents", "tenant_id,user_id", "tenant_members", "tenant_id,user_id", "c", ()),
    ("web_sessions", "tenant_id,user_id", "tenant_members", "tenant_id,user_id", "c", ()),
]
CATALOG_SQL = r"""
BEGIN READ ONLY;
SELECT jsonb_build_object(
  'read_only', current_setting('transaction_read_only'),
  'database', current_database(),
  'migrations', (SELECT jsonb_agg(version ORDER BY version) FROM _sqlx_migrations WHERE success),
  'failed_migrations', (SELECT count(*) FROM _sqlx_migrations WHERE NOT success),
  'migration_checksums', (SELECT jsonb_object_agg(version::text,encode(checksum,'hex'))
                           FROM _sqlx_migrations WHERE success),
  'tenant_root_authorization', (
    SELECT jsonb_build_object('owner', owner_role.rolname,
      'row_security', relation.relrowsecurity, 'force_row_security', relation.relforcerowsecurity,
      'delete_privileges', (
        SELECT jsonb_object_agg(role.rolname,has_table_privilege(role.oid,relation.oid,'DELETE'))
          FROM pg_roles role
         WHERE role.rolname IN ('app_user','tenant_lifecycle','tenant_cleanup','shared_cleanup','tgm_owner')
      ))
      FROM pg_class relation JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace
      JOIN pg_roles owner_role ON owner_role.oid=relation.relowner
     WHERE namespace.nspname='public' AND relation.relname='tenants'
  ),
  'security_definer_functions', COALESCE((
    SELECT jsonb_agg(jsonb_build_object('schema', namespace.nspname, 'name', routine.proname,
      'arguments', pg_get_function_identity_arguments(routine.oid))
      ORDER BY namespace.nspname,routine.proname,routine.oid)
      FROM pg_proc routine JOIN pg_namespace namespace ON namespace.oid=routine.pronamespace
     WHERE namespace.nspname NOT LIKE 'pg\_%' AND namespace.nspname<>'information_schema'
       AND routine.prosecdef
  ), '[]'::jsonb),
  'identity_write_fence', (
    SELECT jsonb_build_object('enabled', trigger_row.tgenabled, 'type', trigger_row.tgtype,
      'invoker', NOT routine.prosecdef, 'volatile', routine.provolatile,
      'function_schema', namespace.nspname, 'function', routine.proname)
      FROM pg_trigger trigger_row JOIN pg_proc routine ON routine.oid=trigger_row.tgfoid
      JOIN pg_namespace namespace ON namespace.oid=routine.pronamespace
     WHERE trigger_row.tgrelid='public.identities'::regclass
       AND trigger_row.tgname='identity_lifecycle_write_fence' AND NOT trigger_row.tgisinternal
  ),
  'tables', (
    SELECT jsonb_agg(jsonb_build_object('schema', namespace.nspname, 'name', relation.relname)
                     ORDER BY relation.relname)
      FROM pg_class relation JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace
     WHERE namespace.nspname='public' AND relation.relkind IN ('r','p')
       AND relation.relname<>'_sqlx_migrations'
  ),
  'foreign_keys', (
    SELECT jsonb_agg(jsonb_build_object(
      'name', constraint_row.conname,
      'child_schema', child_namespace.nspname, 'child', child.relname,
      'parent_schema', parent_namespace.nspname, 'parent', parent.relname,
      'columns', (SELECT jsonb_agg(attribute.attname ORDER BY key.ordinality)
                    FROM unnest(constraint_row.conkey) WITH ORDINALITY key(number,ordinality)
                    JOIN pg_attribute attribute ON attribute.attrelid=child.oid AND attribute.attnum=key.number),
      'parent_columns', (SELECT jsonb_agg(attribute.attname ORDER BY key.ordinality)
                           FROM unnest(constraint_row.confkey) WITH ORDINALITY key(number,ordinality)
                           JOIN pg_attribute attribute ON attribute.attrelid=parent.oid AND attribute.attnum=key.number),
      'delete_action', constraint_row.confdeltype,
      'update_action', constraint_row.confupdtype,
      'match_type', constraint_row.confmatchtype,
      'set_null_columns', COALESCE((SELECT jsonb_agg(attribute.attname ORDER BY key.ordinality)
                                     FROM unnest(constraint_row.confdelsetcols) WITH ORDINALITY key(number,ordinality)
                                     JOIN pg_attribute attribute ON attribute.attrelid=child.oid AND attribute.attnum=key.number), '[]'::jsonb),
      'validated', constraint_row.convalidated, 'deferrable', constraint_row.condeferrable)
      ORDER BY parent.relname,child.relname,constraint_row.conname)
      FROM pg_constraint constraint_row
      JOIN pg_class child ON child.oid=constraint_row.conrelid
      JOIN pg_namespace child_namespace ON child_namespace.oid=child.relnamespace
      JOIN pg_class parent ON parent.oid=constraint_row.confrelid
      JOIN pg_namespace parent_namespace ON parent_namespace.oid=parent.relnamespace
     WHERE constraint_row.contype='f'
       AND (child_namespace.nspname='public' OR parent_namespace.nspname='public')
  ),
  'delete_triggers', COALESCE((
    SELECT jsonb_agg(jsonb_build_object('table', relation.relname, 'name', trigger_row.tgname,
      'enabled', trigger_row.tgenabled, 'function', procedure_row.proname)
      ORDER BY relation.relname,trigger_row.tgname)
      FROM pg_trigger trigger_row JOIN pg_class relation ON relation.oid=trigger_row.tgrelid
      JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace
      JOIN pg_proc procedure_row ON procedure_row.oid=trigger_row.tgfoid
     WHERE namespace.nspname='public' AND NOT trigger_row.tgisinternal
       AND trigger_row.tgtype & 8 <> 0
  ), '[]'::jsonb)
);
ROLLBACK;
"""
FIXTURE_SQL = r"""
BEGIN;
CREATE TEMP TABLE purge_probe_tenants(id bigint PRIMARY KEY);
CREATE TEMP TABLE purge_probe_members(
  tenant_id bigint REFERENCES purge_probe_tenants ON DELETE CASCADE,
  user_id bigint, PRIMARY KEY(tenant_id,user_id));
CREATE TEMP TABLE purge_probe_consents(
  tenant_id bigint REFERENCES purge_probe_tenants ON DELETE CASCADE, id bigint PRIMARY KEY,
  user_id bigint, FOREIGN KEY(tenant_id,user_id) REFERENCES purge_probe_members ON DELETE CASCADE);
CREATE TEMP TABLE purge_probe_log(event text CHECK(event='cleanup.done'));
INSERT INTO purge_probe_tenants VALUES(1);
INSERT INTO purge_probe_members VALUES(1,10);
INSERT INTO purge_probe_consents SELECT 1,number,10 FROM generate_series(1,5001) number;
SAVEPOINT cascade_risk;
DO $$
DECLARE deleted_rows bigint;
BEGIN
  DELETE FROM purge_probe_members WHERE tenant_id=1;
  GET DIAGNOSTICS deleted_rows=ROW_COUNT;
  IF deleted_rows<>1 OR EXISTS(SELECT 1 FROM purge_probe_consents) THEN
    RAISE EXCEPTION 'cascade probe unexpected';
  END IF;
END $$;
ROLLBACK TO SAVEPOINT cascade_risk;
DO $$
DECLARE deleted_rows bigint;
BEGIN
  DELETE FROM purge_probe_tenants WHERE id=1
    AND NOT EXISTS(SELECT 1 FROM purge_probe_members WHERE tenant_id=1)
    AND NOT EXISTS(SELECT 1 FROM purge_probe_consents WHERE tenant_id=1);
  GET DIAGNOSTICS deleted_rows=ROW_COUNT;
  IF deleted_rows<>0 THEN RAISE EXCEPTION 'root guard failed'; END IF;
  DELETE FROM purge_probe_consents WHERE id IN(
    SELECT id FROM purge_probe_consents WHERE tenant_id=1 ORDER BY id LIMIT 5000);
  GET DIAGNOSTICS deleted_rows=ROW_COUNT;
  IF deleted_rows<>5000 OR (SELECT count(*) FROM purge_probe_consents)<>1 THEN
    RAISE EXCEPTION 'consent batch bound failed';
  END IF;
  BEGIN
    DELETE FROM purge_probe_consents WHERE tenant_id=1;
    INSERT INTO purge_probe_log VALUES('reject');
    RAISE EXCEPTION 'log failure not raised';
  EXCEPTION WHEN check_violation THEN
    IF (SELECT count(*) FROM purge_probe_consents)<>1 THEN
      RAISE EXCEPTION 'failed log did not roll back deletion';
    END IF;
  END;
  DELETE FROM purge_probe_consents WHERE tenant_id=1;
  DELETE FROM purge_probe_members WHERE tenant_id=1;
  DELETE FROM purge_probe_tenants WHERE id=1;
END $$;
CREATE TEMP TABLE purge_probe_identities(id bigint PRIMARY KEY,tenant_id bigint);
CREATE TEMP TABLE purge_probe_events(id bigint PRIMARY KEY,
  receiver_identity_id bigint REFERENCES purge_probe_identities ON DELETE CASCADE);
CREATE TEMP TABLE purge_probe_projection(id bigint PRIMARY KEY,tenant_id bigint,
  event_id bigint REFERENCES purge_probe_events ON DELETE NO ACTION);
CREATE TEMP TABLE purge_probe_notifications(id bigint PRIMARY KEY,tenant_id bigint,
  identity_id bigint REFERENCES purge_probe_identities ON DELETE CASCADE);
CREATE TEMP TABLE purge_probe_updates(id bigint PRIMARY KEY,tenant_id bigint,
  bot_id bigint REFERENCES purge_probe_identities ON DELETE CASCADE);
CREATE TEMP TABLE purge_probe_jobs(id bigint PRIMARY KEY,
  event_id bigint REFERENCES purge_probe_events ON DELETE CASCADE,
  receiver_identity_id bigint REFERENCES purge_probe_identities ON DELETE CASCADE);
CREATE TEMP TABLE purge_probe_sources(id bigint PRIMARY KEY,tenant_id bigint,
  identity_id bigint REFERENCES purge_probe_identities ON DELETE RESTRICT);
INSERT INTO purge_probe_identities VALUES(11,1),(22,2);
INSERT INTO purge_probe_events VALUES(101,11),(202,22);
INSERT INTO purge_probe_projection VALUES(1,2,101);
INSERT INTO purge_probe_notifications VALUES(1,2,11);
INSERT INTO purge_probe_updates VALUES(1,2,11);
INSERT INTO purge_probe_jobs VALUES(1,202,11);
INSERT INTO purge_probe_sources VALUES(1,2,11);
DO $$ BEGIN
  BEGIN
    DELETE FROM purge_probe_identities WHERE id=11;
    RAISE EXCEPTION 'protected identity was deleted';
  EXCEPTION WHEN foreign_key_violation THEN
    IF (SELECT count(*) FROM purge_probe_notifications)<>1
       OR (SELECT count(*) FROM purge_probe_jobs)<>1 THEN
      RAISE EXCEPTION 'foreign key failure left partial deletion';
    END IF;
  END;
END $$;
DELETE FROM purge_probe_projection;
DELETE FROM purge_probe_sources;
SAVEPOINT cross_tenant_cascade;
DELETE FROM purge_probe_identities WHERE id=11;
DO $$ BEGIN
  IF EXISTS(SELECT 1 FROM purge_probe_notifications)
     OR EXISTS(SELECT 1 FROM purge_probe_updates)
     OR EXISTS(SELECT 1 FROM purge_probe_jobs)
     OR NOT EXISTS(SELECT 1 FROM purge_probe_events WHERE id=202) THEN
    RAISE EXCEPTION 'cross-tenant cascade probe unexpected';
  END IF;
END $$;
ROLLBACK TO SAVEPOINT cross_tenant_cascade;
DO $$ BEGIN
  DELETE FROM purge_probe_identities WHERE id=11
    AND NOT EXISTS(SELECT 1 FROM purge_probe_events WHERE receiver_identity_id=11)
    AND NOT EXISTS(SELECT 1 FROM purge_probe_notifications WHERE identity_id=11)
    AND NOT EXISTS(SELECT 1 FROM purge_probe_updates WHERE bot_id=11)
    AND NOT EXISTS(SELECT 1 FROM purge_probe_jobs WHERE receiver_identity_id=11)
    AND NOT EXISTS(SELECT 1 FROM purge_probe_sources WHERE identity_id=11);
  IF NOT EXISTS(SELECT 1 FROM purge_probe_identities WHERE id=11) THEN
    RAISE EXCEPTION 'identity dependency guard failed';
  END IF;
END $$;
SELECT jsonb_build_object('role',current_user,'superuser',
  (SELECT rolsuper FROM pg_roles WHERE rolname=current_user),
  'bypassrls',(SELECT rolbypassrls FROM pg_roles WHERE rolname=current_user),
  'cascade_exceeds_parent_batch',true,'bounded_child_cleanup',true,
  'log_failure_rolls_back',true,'foreign_key_failure_is_atomic',true,
  'cross_tenant_cascade_requires_guard',true,'production_purge_verified',false);
ROLLBACK;
"""
IDENTITY_FENCE_SQL = r"""
BEGIN;
DO $$
DECLARE tenant_key bigint; identity_key bigint;
BEGIN
  IF current_database()<>'tgm_test_schema' OR current_user<>'app_user'
    OR NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname=current_user AND NOT rolsuper AND NOT rolbypassrls)
  THEN
    RAISE EXCEPTION 'identity fence probe requires restricted app_user on tgm_test_schema';
  END IF;
  INSERT INTO tenants(name,owner_user_id)
    VALUES('purge-identity-fence-probe',-905541) RETURNING id INTO tenant_key;
  PERFORM set_config('app.tenant_id',tenant_key::text,true);
  INSERT INTO identities(kind,tenant_id,status) VALUES('bot',tenant_key,'unauthorized');
  INSERT INTO tenants(name,owner_user_id,status,deleted_at)
    VALUES('purge-identity-fence-probe',-905541,'deleted',statement_timestamp()-interval '31 days')
    RETURNING id INTO tenant_key;
  PERFORM set_config('app.tenant_id',tenant_key::text,true);
  IF EXISTS(SELECT 1 FROM identities WHERE tenant_id=tenant_key) THEN
    RAISE EXCEPTION 'identity fence probe requires an empty tenant';
  END IF;
  PERFORM set_config('tgm_probe.canonical_insert','not_attempted',true);
  BEGIN
    INSERT INTO identities(kind,tenant_id,status) VALUES('bot',tenant_key,'unauthorized')
      RETURNING id INTO identity_key;
    PERFORM set_config('tgm_probe.identity_insert','accepted',true);
  EXCEPTION WHEN insufficient_privilege THEN
    PERFORM set_config('tgm_probe.identity_insert','rejected',true);
  END;
  IF current_setting('tgm_probe.identity_insert')='accepted' THEN
    BEGIN
      INSERT INTO canonical_events(adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date)
        VALUES('bot',identity_key,-905541,1,statement_timestamp());
      PERFORM set_config('tgm_probe.canonical_insert','accepted',true);
    EXCEPTION WHEN insufficient_privilege THEN
      PERFORM set_config('tgm_probe.canonical_insert','rejected',true);
    END;
  END IF;
END $$;
SELECT jsonb_build_object('database',current_database(),'role',current_user,
  'superuser',(SELECT rolsuper FROM pg_roles WHERE rolname=current_user),
  'bypassrls',(SELECT rolbypassrls FROM pg_roles WHERE rolname=current_user),
  'active_identity_inserted',true,
  'deleted_identity_inserted',current_setting('tgm_probe.identity_insert')='accepted',
  'deleted_canonical_inserted',current_setting('tgm_probe.canonical_insert')='accepted',
  'deleted_identity_blocked',current_setting('tgm_probe.identity_insert')='rejected',
  'all_write_paths_verified',false,'physical_delete_implemented',false);
ROLLBACK;
"""


def expected_edges():
    result = {
        ("public", child, ("tenant_id",), "public", "tenants", ("id",), "c", ())
        for child in TENANT_CHILDREN
    }
    result.update(
        ("public", child, (column,), "public", "identities", ("id",), action, ())
        for child, (column, action) in IDENTITY_CHILDREN.items()
    )
    result.update(
        ("public", child, tuple(columns.split(",")), "public", parent,
         tuple(parent_columns.split(",")), action, set_null)
        for child, columns, parent, parent_columns, action, set_null in OTHER_EDGES
    )
    return result


def edge_key(edge):
    return (edge["child_schema"], edge["child"], tuple(edge["columns"]),
            edge["parent_schema"], edge["parent"], tuple(edge["parent_columns"]),
            edge["delete_action"], tuple(edge["set_null_columns"]))


def validate_catalog(snapshot, versions, checksums):
    if snapshot["read_only"] != "on":
        raise ValueError("目录连接不是只读事务")
    if snapshot["migrations"] != sorted(versions):
        raise ValueError("本地库迁移版本与工作树不一致")
    if snapshot["failed_migrations"]:
        raise ValueError("本地库存在未成功的迁移记录")
    if snapshot["migration_checksums"] != checksums:
        raise ValueError("已应用迁移的 checksum 与工作树不一致")
    if snapshot["tenant_root_authorization"] != ROOT_AUTHORIZATION:
        raise ValueError("租户根表授权基线漂移，须重新审核最终删除入口")
    if snapshot["security_definer_functions"]:
        raise ValueError("发现 SECURITY DEFINER 对象，不符合当前 A8 基线")
    tables = {(table["schema"], table["name"]) for table in snapshot["tables"]}
    if tables != {("public", table) for table in TABLES}:
        raise ValueError("实际表清单已改变，须重新审核清理范围")
    edges = snapshot["foreign_keys"]
    actual = {edge_key(edge) for edge in edges}
    expected = expected_edges()
    if actual != expected or len(edges) != len(expected):
        raise ValueError(f"外键拓扑漂移：missing={sorted(expected-actual)!r}; unexpected={sorted(actual-expected)!r}")
    for edge in edges:
        if not edge["validated"] or edge["deferrable"] or edge["update_action"] != "a" or edge["match_type"] != "s":
            raise ValueError("外键校验、延迟或更新语义已改变")
    if snapshot["delete_triggers"] != [{
        "table": "identity_secrets", "name": "tenant_lifecycle_secret_delete",
        "enabled": "O", "function": "tenant_lifecycle_secret_delete",
    }]:
        raise ValueError("删除触发器清单漂移，须人工核查")
    if snapshot["identity_write_fence"] != {
        "enabled": "O", "type": 23, "invoker": True, "volatile": "v",
        "function_schema": "public", "function": "identity_lifecycle_write_fence",
    }:
        raise ValueError("身份写入围栏目录漂移，须运行 C15 核查")
    return {
        "catalog_contract": "pass",
        "tenant_children": sorted(TENANT_CHILDREN),
        "identity_children": sorted(IDENTITY_CHILDREN),
        "cascade_parents_requiring_child_batches": sorted({
            edge["parent"] for edge in edges if edge["delete_action"] == "c"
        }),
        "physical_delete_implemented": False,
        "permission_audit_verified": False,
        "all_copies_deleted": False,
        "root_delete_authorization": "requires_approved_entrypoint",
    }


def docker_container(container):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", container):
        raise ValueError("容器名称无效")
    result = subprocess.run(DOCKER + ["inspect", "--format",
                            '{{.Id}} {{.State.Running}} {{index .Config.Labels "com.docker.compose.service"}}',
                            container], check=True, text=True, capture_output=True)
    identity, running, service = result.stdout.strip().split()
    if service != "postgres" or running != "true":
        raise ValueError("仅允许运行中的本地 Compose postgres 服务")
    return identity


def psql_command(container, database, role="postgres", read_only=True):
    options = "-c statement_timeout=10000 -c lock_timeout=2000 -c idle_in_transaction_session_timeout=15000"
    if read_only:
        options += " -c default_transaction_read_only=on"
    return DOCKER + ["exec", "-i", "-e", f"PGOPTIONS={options}", container,
                     "psql", "-X", "--no-password", "-qAt", "-v", "ON_ERROR_STOP=1",
                     "-h", "/var/run/postgresql", "-p", "5432", "-U", role, "-d", database]


def run_sql(container, database, sql, role="postgres", read_only=True):
    result = subprocess.run(psql_command(container, database, role, read_only),
                            input=sql, check=True, text=True, capture_output=True, timeout=25)
    return json.loads(result.stdout)


def probe_locks(container):
    tenant = -int(uuid.uuid4().hex[:12], 16)
    keys = [
        (f"tenant_lifecycle:{tenant}", 419871),
        (f"tenant_retention:{tenant}:event_lifecycle", 1952935218),
        ("shared_retention:canonical_events", 1952935218),
        (f"shared_retention:identity:{tenant}", 1952935218),
        (f"media-group:{tenant}:-1001:probe", 0),
    ]
    expressions = [f"hashtextextended('{key}',{seed})" for key, seed in keys]
    holder = subprocess.Popen(psql_command(container, "tgm_test_control", "tgm_test", False),
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
    selector = selectors.DefaultSelector()
    try:
        holder.stdin.write("BEGIN;\n" + "\n".join(
            f"SELECT pg_advisory_xact_lock({expression});" for expression in expressions
        ) + "\nSELECT 'ready';\n")
        holder.stdin.flush()
        selector.register(holder.stdout, selectors.EVENT_READ)
        deadline = time.monotonic() + 10
        ready = False
        pending = b""
        while not ready and time.monotonic() < deadline:
            if selector.select(timeout=max(0, deadline-time.monotonic())):
                chunk = os.read(holder.stdout.fileno(), 4096)
                if not chunk:
                    raise ValueError("锁持有连接提前退出")
                pending += chunk
                lines = pending.split(b"\n")
                pending = lines.pop()
                ready = any(line.strip() == b"ready" for line in lines)
        if not ready:
            raise ValueError("锁探针等待超时")
        attempts = [f"pg_try_advisory_xact_lock_shared({expression})" for expression in expressions]
        query = "BEGIN; SELECT jsonb_build_array(" + ",".join(attempts) + "); ROLLBACK;"
        blocked = run_sql(container, "tgm_test_control", query, "tgm_test", False)
        if blocked != [False] * len(keys):
            raise ValueError("排他锁未阻止并发共享锁")
        holder.stdin.write("ROLLBACK;\n\\q\n")
        holder.stdin.flush()
        holder.wait(timeout=10)
        if holder.returncode:
            raise ValueError("锁探针事务未正常回滚")
        released = run_sql(container, "tgm_test_control", query, "tgm_test", False)
        if released != [True] * len(keys):
            raise ValueError("事务结束后锁未释放")
        return {"conflicts_detected": True, "rollback_releases_locks": True,
                "production_lock_protocol_verified": False}
    finally:
        selector.close()
        if holder.poll() is None:
            holder.kill()
        holder.communicate(timeout=10)


def fixture_catalog():
    edges = [{"name": f"fixture_{index}", "child_schema": child_schema, "child": child,
              "columns": list(columns), "parent_schema": parent_schema, "parent": parent,
              "parent_columns": list(parent_columns), "delete_action": action,
              "set_null_columns": list(set_null), "validated": True,
              "deferrable": False, "update_action": "a", "match_type": "s"}
             for index, (child_schema, child, columns, parent_schema, parent, parent_columns,
                         action, set_null) in enumerate(sorted(expected_edges()))]
    return {"read_only": "on", "migrations": [54], "failed_migrations": 0,
            "migration_checksums": {"54": "fixture"},
            "tenant_root_authorization": {**ROOT_AUTHORIZATION,
                "delete_privileges": dict(ROOT_AUTHORIZATION["delete_privileges"])},
            "security_definer_functions": [],
            "identity_write_fence": {"enabled": "O", "type": 23, "invoker": True,
                "volatile": "v", "function_schema": "public", "function": "identity_lifecycle_write_fence"},
            "tables": [{"schema": "public", "name": table} for table in sorted(TABLES)],
            "foreign_keys": edges,
            "delete_triggers": [{"table": "identity_secrets", "name": "tenant_lifecycle_secret_delete",
                                 "enabled": "O", "function": "tenant_lifecycle_secret_delete"}]}


class CatalogTests(unittest.TestCase):
    def test_baseline_is_not_a_purge_or_permission_guarantee(self):
        result = validate_catalog(fixture_catalog(), [54], {"54": "fixture"})
        self.assertEqual(result["catalog_contract"], "pass")
        self.assertFalse(result["physical_delete_implemented"])
        self.assertFalse(result["permission_audit_verified"])

    def test_unknown_table_or_external_reference_is_rejected(self):
        for mutation in ["table", "schema"]:
            with self.subTest(mutation=mutation):
                snapshot = fixture_catalog()
                if mutation == "table":
                    snapshot["tables"].append({"schema": "public", "name": "new_private_data"})
                else:
                    snapshot["foreign_keys"][0]["child_schema"] = "other_schema"
                with self.assertRaises(ValueError):
                    validate_catalog(snapshot, [54], {"54": "fixture"})

    def test_fk_semantic_changes_fail_closed(self):
        changes = [("delete_action", "c"), ("validated", False), ("deferrable", True),
                   ("update_action", "c"), ("match_type", "f"), ("set_null_columns", [])]
        for field, value in changes:
            with self.subTest(field=field):
                snapshot = fixture_catalog()
                edge = next(edge for edge in snapshot["foreign_keys"] if edge["delete_action"] == "n")
                edge[field] = value
                with self.assertRaises(ValueError):
                    validate_catalog(snapshot, [54], {"54": "fixture"})

    def test_missing_or_duplicate_fk_is_rejected(self):
        for duplicate in [False, True]:
            snapshot = fixture_catalog()
            if duplicate:
                snapshot["foreign_keys"].append(snapshot["foreign_keys"][0].copy())
            else:
                snapshot["foreign_keys"].pop()
            with self.assertRaises(ValueError):
                validate_catalog(snapshot, [54], {"54": "fixture"})

    def test_trigger_or_migration_drift_is_rejected(self):
        for field, value in [("delete_triggers", []), ("migrations", [53]), ("read_only", "off"),
                             ("failed_migrations", 1),
                             ("migration_checksums", {"54": "modified"})]:
            snapshot = fixture_catalog()
            snapshot[field] = value
            with self.assertRaises(ValueError):
                validate_catalog(snapshot, [54], {"54": "fixture"})

    def test_single_column_cascades_are_in_the_contract(self):
        for table in ["tenant_notifications", "bot_console_updates", "media_relay_jobs"]:
            self.assertEqual(IDENTITY_CHILDREN[table][1], "c")

    def test_root_authorization_drift_is_rejected(self):
        for field, value in [("owner", "app_user"), ("row_security", True),
                             ("force_row_security", True)]:
            with self.subTest(field=field):
                snapshot = fixture_catalog()
                snapshot["tenant_root_authorization"][field] = value
                with self.assertRaises(ValueError):
                    validate_catalog(snapshot, [54], {"54": "fixture"})
        for role in ROOT_AUTHORIZATION["delete_privileges"]:
            with self.subTest(role=role):
                snapshot = fixture_catalog()
                privileges = snapshot["tenant_root_authorization"]["delete_privileges"]
                privileges[role] = not privileges[role]
                with self.assertRaises(ValueError):
                    validate_catalog(snapshot, [54], {"54": "fixture"})
        snapshot = fixture_catalog()
        snapshot["security_definer_functions"] = [{"schema": "public", "name": "unexpected"}]
        with self.assertRaises(ValueError):
            validate_catalog(snapshot, [54], {"54": "fixture"})

    def test_fixture_mutates_only_temporary_probe_tables(self):
        tables = re.findall(r"(?:CREATE TEMP TABLE|INSERT INTO|DELETE FROM)\s+(\w+)", FIXTURE_SQL)
        self.assertTrue(tables)
        self.assertTrue(all(table.startswith("purge_probe_") for table in tables))
        self.assertTrue(FIXTURE_SQL.rstrip().endswith("ROLLBACK;"))
        self.assertIn("BEGIN READ ONLY;", CATALOG_SQL)
        self.assertTrue(CATALOG_SQL.rstrip().endswith("ROLLBACK;"))

    def test_identity_probe_is_schema_bound_and_rolls_back(self):
        self.assertIn("current_database()<>'tgm_test_schema'", IDENTITY_FENCE_SQL)
        self.assertIn("current_user<>'app_user'", IDENTITY_FENCE_SQL)
        self.assertTrue(IDENTITY_FENCE_SQL.lstrip().startswith("BEGIN;"))
        self.assertTrue(IDENTITY_FENCE_SQL.rstrip().endswith("ROLLBACK;"))
        self.assertEqual(set(re.findall(r"INSERT INTO\s+(\w+)", IDENTITY_FENCE_SQL)),
                         {"tenants", "identities", "canonical_events"})
        self.assertIn("EXCEPTION WHEN insufficient_privilege", IDENTITY_FENCE_SQL)
        self.assertNotIn("WHEN OTHERS", IDENTITY_FENCE_SQL)

    def test_identity_trigger_drift_is_rejected(self):
        for field, value in [("enabled", "D"), ("type", 7), ("invoker", False),
                             ("volatile", "s"), ("function_schema", "other")]:
            snapshot = fixture_catalog()
            snapshot["identity_write_fence"][field] = value
            with self.assertRaises(ValueError):
                validate_catalog(snapshot, [54], {"54": "fixture"})


def main():
    parser = argparse.ArgumentParser(description="本地租户物理清理前置核验；不执行业务数据清理")
    parser.add_argument("--container", help="现有本地 Compose postgres 容器")
    parser.add_argument("--database", choices=["tgm", "tgm_test_schema"], default="tgm")
    parser.add_argument("--probe-safety", action="store_true", help="在 tgm_test_control 使用临时表和 tgm_test 运行回滚探针")
    parser.add_argument("--probe-identity-fence", action="store_true", help="仅在 tgm_test_schema 以 app_user 回滚验证注销身份写入围栏")
    parser.add_argument("--self-test", action="store_true", help="不连接 Docker，运行目录契约反向测试")
    arguments = parser.parse_args()
    if arguments.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(CatalogTests)
        ).wasSuccessful() else 1
    if not arguments.container:
        parser.error("目录核验需要显式指定 --container")
    container = docker_container(arguments.container)
    snapshot = run_sql(container, arguments.database, CATALOG_SQL)
    checksums = {str(int(re.match(r"(\d+)_", migration.name).group(1))):
                 hashlib.sha384(migration.read_bytes()).hexdigest()
                 for migration in (ROOT / "migrations").glob("*.sql")}
    versions = [int(version) for version in checksums]
    result = validate_catalog(snapshot, versions, checksums)
    result["catalog"] = snapshot
    if arguments.probe_identity_fence:
        if arguments.database != "tgm_test_schema":
            schema_snapshot = run_sql(container, "tgm_test_schema", CATALOG_SQL)
            validate_catalog(schema_snapshot, versions, checksums)
        result["identity_write_fence_probe"] = run_sql(
            container, "tgm_test_schema", IDENTITY_FENCE_SQL, "app_user", False)
    if arguments.probe_safety:
        probe = run_sql(container, "tgm_test_control", FIXTURE_SQL, "tgm_test", False)
        if probe["role"] != "tgm_test" or probe["superuser"] or probe["bypassrls"]:
            raise ValueError("临时表探针没有使用受限测试角色")
        result["rollback_fixture_probes"] = probe
        result["lock_probes"] = probe_locks(container)
    if arguments.probe_identity_fence:
        blocked = result["identity_write_fence_probe"]["deleted_identity_blocked"]
        result["terminal_cleanup_precondition"] = "requires_concurrency_validation" if blocked else "failed"
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 2 if arguments.probe_identity_fence and not blocked else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        print(f"前置核验失败：{error}", file=sys.stderr)
        sys.exit(1)
