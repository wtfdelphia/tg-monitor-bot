import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
import uuid


ROOT = Path(__file__).resolve().parents[2]
DRIVER = (ROOT / "scripts/ai-driver/ai-run.sh").read_text()
GATES = ROOT / "scripts/ai-driver/ai-gates.sh"
SYSTEMD = "--systemd" in sys.argv
if SYSTEMD:
    sys.argv.remove("--systemd")
FRESH_DB = "--fresh-db" in sys.argv
if FRESH_DB:
    sys.argv.remove("--fresh-db")


def definitions(source, *names):
    return "\n".join(
        re.search(rf"(?ms)^{name}\(\) \{{\n.*?^\}}$", source).group()
        for name in names
    )


class Regressions(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="ai-driver-regressions-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.home = self.root / "home"
        self.home.mkdir()
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_COUNT": "0",
        }
        for key in list(self.env):
            if key.startswith("TGM_") or key in ("DATABASE_URL", "CARGO_TARGET_DIR"):
                del self.env[key]
        self.git("init", "-q")
        (self.repo / "AGENTS.md").write_text("baseline\n")
        (self.repo / "CLAUDE.md").symlink_to("AGENTS.md")
        (self.repo / "newline-link").symlink_to("missing\n")
        (self.repo / "scripts").mkdir()
        (self.repo / "scripts/tool.sh").write_text("baseline\n")
        self.git("add", ".")
        self.git("-c", "user.name=regression", "-c", "user.email=regression@example.invalid",
                 "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")
        self.env.update({
            "COMMON_GIT_DIR": str(self.repo / ".git"),
            "PRIMARY": str(self.repo),
            "wt": str(self.repo),
            "STATE_DIR": str(self.root / "state"),
            "TRUSTED_ROOT": str(self.repo),
        })
        self.fingerprint = definitions(
            DRIVER, "fingerprint_path_tree", "primary_guard_fingerprint",
            "state_fingerprint", "check_state_dir", "check_trusted_source_snapshot",
        )

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.repo), *args], env=self.env,
                              text=True, capture_output=True, check=True).stdout.strip()

    def bash(self, source, body, expected=0, stdin=None):
        result = subprocess.run(
            ["/bin/bash", "-c", "set -euo pipefail\n"
             'die() { echo "$*" >&2; exit 1; }\n' + source + "\n" + body],
            env=self.env, cwd=self.repo, text=True, input=stdin,
            capture_output=True, timeout=30,
        )
        if expected is not None:
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        return result

    def protected(self, expected=0):
        source = definitions(DRIVER, "protected_tree_mismatches")
        return self.bash(source, 'PROTECTED=(AGENTS.md CLAUDE.md newline-link scripts); '
                         'protected_tree_mismatches "$PRIMARY" HEAD', expected)

    def test_protected_paths_compare_raw_bytes_and_types(self):
        self.assertEqual(self.protected().stdout, "")
        agent = self.repo / "AGENTS.md"
        agent.write_text("changed\n")
        self.assertIn("AGENTS.md", self.protected().stdout)
        agent.write_text("baseline\n")
        agent.chmod(0o755)
        self.assertIn("AGENTS.md", self.protected().stdout)
        agent.chmod(0o644)
        agent.unlink()
        self.assertIn("AGENTS.md", self.protected().stdout)
        agent.symlink_to("scripts/tool.sh")
        self.assertIn("AGENTS.md", self.protected().stdout)
        agent.unlink()
        agent.write_text("baseline\n")
        link = self.repo / "CLAUDE.md"
        link.unlink()
        link.symlink_to("scripts/tool.sh")
        self.assertIn("CLAUDE.md", self.protected().stdout)
        (self.repo / "scripts/new.sh").write_text("new\n")
        self.assertIn("scripts/new.sh", self.protected().stdout)

    def test_read_errors_fail_closed(self):
        bindir = self.root / "bin"
        bindir.mkdir()
        self.env["PATH"] = f"{bindir}:{self.env['PATH']}"
        for tool, body in (("readlink", "exit 1"), ("find", "exit 1"),
                           ("sha256sum", "exit 1"), ("stat", "exit 1")):
            with self.subTest(tool=tool):
                launcher = bindir / tool
                launcher.write_text(f"#!/bin/bash\n{body}\n")
                launcher.chmod(0o755)
                if tool in ("readlink", "find"):
                    self.assertNotEqual(self.protected(expected=None).returncode, 0)
                result = self.bash(self.fingerprint,
                                   'fingerprint_path_tree "$PRIMARY/CLAUDE.md" test',
                                   expected=None)
                self.assertNotEqual(result.returncode, 0)
                launcher.unlink()

    def test_fingerprints_accept_files_links_and_directories(self):
        body = 'PROTECTED=(AGENTS.md CLAUDE.md newline-link scripts); primary_guard_fingerprint'
        baseline = self.bash(self.fingerprint, body).stdout
        self.assertEqual(baseline, self.bash(self.fingerprint, body).stdout)
        for path, change in (
            (self.repo / "AGENTS.md", lambda path: path.write_text("changed\n")),
            (self.repo / "scripts/tool.sh", lambda path: path.chmod(0o755)),
            (self.repo / "newline-link", lambda path: (path.unlink(), path.symlink_to("missing"))),
        ):
            with self.subTest(path=path):
                change(path)
                self.assertNotEqual(baseline, self.bash(self.fingerprint, body).stdout)
                self.git("checkout", "HEAD", "--", str(path.relative_to(self.repo)))

    def test_trust_roots_still_reject_files_and_symlinks(self):
        state = Path(self.env["STATE_DIR"])
        for kind in ("file", "symlink"):
            with self.subTest(kind=kind):
                if kind == "file":
                    state.write_text("not a directory")
                else:
                    state.symlink_to(self.repo)
                self.bash(self.fingerprint, "state_fingerprint", expected=1)
                self.env["TRUSTED_ROOT"] = str(state)
                self.bash(self.fingerprint, "TRUSTED_SOURCE_SNAPSHOT=''; check_trusted_source_snapshot",
                          expected=1)
                state.unlink()
        guard = definitions(DRIVER, "config_file_signature", "check_config_include_files",
                            "read_config_values", "git_guard_fingerprint")
        hooks = self.root / "hooks"
        self.git("config", "core.hooksPath", str(hooks))
        self.env.update(HOST_GIT_CONFIG_GLOBAL="/dev/null", HOST_GIT_CONFIG_SYSTEM="/dev/null")
        for kind in ("file", "symlink"):
            with self.subTest(hooks=kind):
                if kind == "file":
                    hooks.write_text("not a directory")
                else:
                    hooks.symlink_to(self.repo)
                self.bash(guard + "\n" + self.fingerprint, "git_guard_fingerprint", expected=1)
                hooks.unlink()

    def test_retry_prompt_contains_feedback_and_never_overwrites(self):
        source = definitions(DRIVER, "safe_write_log", "build_prompt")
        task = self.root / "task.md"
        task.write_text("id: regression\nstatus: pending\n")
        self.env.update(TASK_FILE=str(task), TASK_ID="regression", TS="fixed",
                        LOG_DIR=str(self.root), ESCALATION="no",
                        PROMPT_FILE=str(self.root / "first.prompt.md"),
                        GATE_LOG=str(self.root / "gate.log"))
        initial = re.search(r'^build_prompt ""[^\n]*$', DRIVER, re.M).group()
        self.bash(source, initial)
        baseline = Path(self.env["PROMPT_FILE"]).read_bytes()
        Path(self.env["GATE_LOG"]).write_text("failure-from-first-gate\n")
        retry = re.search(r'(?ms)^  log "卡口红[^\n]*\n(.*?)^  RUN_LOG=[^\n]*$',
                          DRIVER).group(1)
        for attempt in (1, 2):
            self.bash(source, f"attempt={attempt}\n" + retry)
            prompt = self.root / f"regression.fixed.retry{attempt}.prompt.md"
            self.assertIn("failure-from-first-gate", prompt.read_text())
        self.assertEqual(Path(self.env["PROMPT_FILE"]).read_bytes(), baseline)
        marker = self.root / "executor-started"
        result = self.bash(source, "attempt=1\n" + retry +
                           f"\ntouch {shlex.quote(str(marker))}", expected=1)
        self.assertIn("日志目标已存在", result.stderr)
        self.assertFalse(marker.exists())
        linked_prompt = self.root / "regression.fixed.retry3.prompt.md"
        linked_prompt.symlink_to(self.root / "missing")
        self.bash(source, "attempt=3\n" + retry +
                  f"\ntouch {shlex.quote(str(marker))}", expected=1)
        self.assertFalse(marker.exists())
        self.bash(source, initial + f"\ntouch {shlex.quote(str(marker))}", expected=1)
        self.assertFalse(marker.exists())

    def gate_trace(self, *args, overrides=None, expected=0):
        bindir = self.root / "gate-bin"
        bindir.mkdir(exist_ok=True)
        trace = self.root / "trace.jsonl"
        trace.write_text("")
        launcher = '#!/usr/bin/python3\n' + (
            "import json, os, sys\n"
            "if '--version' in sys.argv: sys.exit(0)\n"
            "with open(os.environ['REGRESSION_TRACE'], 'a') as output:\n"
            "    output.write(json.dumps({'tool': os.path.basename(sys.argv[0]), "
            "'args': sys.argv[1:], 'env': dict(os.environ)}) + '\\n')\n"
        )
        for tool in ("cargo", "psql"):
            target = bindir / tool
            target.write_text(launcher)
            target.chmod(0o755)
        for helper in ("check-newtype.sh", "migrate-idempotent.sh", "audit-double-pass.sh"):
            target = self.repo / "scripts" / helper
            target.write_text(launcher)
            target.chmod(0o755)
        (self.repo / ".github/workflows").mkdir(parents=True, exist_ok=True)
        (self.repo / ".github/workflows/ci.yml").write_text("")
        env = {**self.env, "PATH": f"{bindir}:{self.env['PATH']}",
               "REGRESSION_TRACE": str(trace), "DATABASE_URL": "postgres://test/control",
               "TGM_DB_URL_OWNER": "postgres://owner/production", **(overrides or {})}
        result = subprocess.run(["/bin/bash", str(GATES), *args], env=env, cwd=self.repo,
                                text=True, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        events = [json.loads(line) for line in trace.read_text().splitlines()]
        return result, events

    def test_gate_order_modes_and_schema_defaults(self):
        order = re.search(r'ORDER=\((.*?)\)', GATES.read_text()).group(1).split()
        for args, selected in (
            ((), order),
            (("--static-only",), order[:5]),
            (("--no-db",), order[:5]),
            (("--task-only",), [*order[:5], "sqlx-check", "test"]),
            (("--trusted-db-only",), ["migrate", "migrate-idempotent", "audit-rls", "double-pass"]),
            (("--from", "migrate"), order[5:]),
            (("--task-only", "--from", "sqlx-check"), ["sqlx-check", "test"]),
            (("--from", "test"), order[8:]),
        ):
            with self.subTest(args=args):
                result, events = self.gate_trace(*args)
                self.assertEqual(re.findall(r'^── 卡口 (\S+)', result.stdout, re.M), selected)
                if "--from" in args:
                    self.assertNotIn("全部本地卡口通过", result.stdout)
                if not args:
                    migrations = [event for event in events if event["args"][-1:] == ["migrate"]]
                    production = next(event for event in migrations if
                                      event["env"]["DATABASE_URL"] == "postgres://owner/production")
                    test = next(event for event in events if event["args"] == ["test", "--workspace"])
                    self.assertLess(events.index(production), events.index(test))
                for event in events:
                    if event["tool"] == "cargo" and event["args"][:2] == ["sqlx", "prepare"]:
                        self.assertEqual(event["env"]["DATABASE_URL"],
                                         "postgres://app_user:apw@127.0.0.1:55432/tgm_test_schema")
                    if event["tool"] == "cargo" and event["args"][:1] in (["sqlx"], ["test"], ["fmt"], ["clippy"], ["build"], ["deny"]):
                        self.assertNotIn("TGM_DB_URL_OWNER", event["env"])
                        self.assertNotIn("TGM_TASK_SCHEMA_OWNER_DATABASE_URL", event["env"])
        for args in (("--from", "invalid"), ("--static-only", "--from", "test"),
                     ("--task-only", "--from", "migrate"), ("--trusted-db-only", "--from", "test"),
                     ("--task-only", "--no-db")):
            self.gate_trace(*args, expected=2)

    def test_explicit_schema_and_control_overrides(self):
        _, events = self.gate_trace("--task-only", "--from", "sqlx-check", overrides={
            "TGM_DB_URL_TEST": "postgres://override/control",
            "TGM_TEST_DATABASE_URL": "postgres://lower-priority/control",
            "TGM_DB_URL_TASK_SCHEMA": "postgres://override/schema",
            "TGM_TASK_SCHEMA_DATABASE_URL": "postgres://lower-priority/schema",
            "TGM_DB_URL_TASK_SCHEMA_OWNER": "postgres://override/schema-owner",
        })
        for command, url in ((["sqlx", "prepare", "--workspace", "--check"], "postgres://override/schema"),
                             (["test", "--workspace"], "postgres://override/control")):
            event = next(event for event in events if event["args"] == command)
            self.assertEqual(event["env"]["DATABASE_URL"], url)
        migration = next(event for event in events if event["args"][-1:] == ["migrate"])
        self.assertEqual(migration["env"]["DATABASE_URL"], "postgres://override/schema-owner")

    @unittest.skipUnless(SYSTEMD, "使用 --systemd 验证真实用户服务")
    def test_actual_services_receive_only_whitelisted_environment(self):
        secret = "TGM_REGRESSION_SECRET_" + uuid.uuid4().hex.upper()
        subprocess.run(["systemctl", "--user", "set-environment", f"{secret}=manager-value"], check=True)
        self.addCleanup(subprocess.run, ["systemctl", "--user", "unset-environment", secret], check=True)
        self.env.update({secret: "client-value", "DATABASE_URL": "owner-must-not-leak",
                         "TGM_DB_URL_OWNER": "owner-must-not-leak"})
        rustc = subprocess.run(["rustup", "which", "rustc"], text=True, capture_output=True,
                               check=True).stdout.strip()
        bindir = self.root / "service-bin"
        bindir.mkdir()
        rustup = bindir / "rustup"
        rustup.write_text("#!/bin/bash\nprintf '%s\\n' " + shlex.quote(rustc) + "\n")
        rustup.chmod(0o755)
        self.env.update(PATH=f"{bindir}:{self.env['PATH']}", TARGET_ROOT=str(self.root / "target"),
                        EXECUTOR_TARGET_DIR=str(self.root / "executor-target"), EXECUTOR_DATABASE_URL="",
                        EXECUTOR_GIT_USER_NAME="price$HOME ${HOME} $$ space",
                        EXECUTOR_GIT_USER_EMAIL="probe@example.invalid", TIMEOUT="10",
                        LOG_DIR=str(self.root), TASK_ID="service", TS="probe",
                        GATES_TARGET_DIR=str(self.root / "gates"), TRUSTED_TARGET_DIR=str(self.root / "trusted"),
                        TASK_DATABASE_URL="postgres://test/control", TASK_SCHEMA_DATABASE_URL="postgres://app/schema",
                        TASK_SCHEMA_OWNER_DATABASE_URL="postgres://owner/schema", DRIVER_OWNER_DB_URL="postgres://owner/main",
                        BASE_COMMIT=self.git("rev-parse", "HEAD"))
        probe = 'import json, os, sys; print(json.dumps({"env": dict(os.environ), "cwd": os.getcwd(), "args": sys.argv[1:]}))'
        source = definitions(DRIVER, "executor_env", "run_gates", "wait_scope_empty")
        body = 'executor_env "ai-regression-executor-$$" /usr/bin/python3 -c ' + shlex.quote(probe)
        body += " " + shlex.quote("$HOME ${HOME} $$ literal")
        result = self.bash(source, body)
        actual = json.loads(result.stdout)
        self.assertEqual(actual["cwd"], str(self.repo))
        self.assertEqual(actual["args"], ["$HOME ${HOME} $$ literal"])
        self.assertEqual(actual["env"]["HOME"], str(self.root / "target/executor-home"))
        self.assertEqual(actual["env"]["CARGO_TARGET_DIR"], self.env["EXECUTOR_TARGET_DIR"])
        self.assertEqual(actual["env"]["GIT_CONFIG_VALUE_0"], self.env["EXECUTOR_GIT_USER_NAME"])
        for key in (secret, "DATABASE_URL", "TGM_DB_URL_OWNER"):
            self.assertNotIn(key, actual["env"])
        self.env["GATES_SCRIPT"] = definitions(GATES.read_text(), "run_task", "run_static") + "\n" + (
            'TASK_DB_URL="$TGM_TEST_DATABASE_URL"; TASK_TARGET_DIR="$TGM_TASK_TARGET_DIR"\n'
            'run_task test /usr/bin/python3 -c ' + shlex.quote(probe) + "\n"
            'run_static /usr/bin/python3 -c ' + shlex.quote(probe)
        )
        output = self.bash(source, "if run_gates; then :; else exit $?; fi").stdout
        task, static = [json.loads(line)["env"] for line in output.splitlines() if line.startswith("{")]
        self.assertEqual(task["DATABASE_URL"], self.env["TASK_DATABASE_URL"])
        self.assertEqual(task["CARGO_TARGET_DIR"], str(self.root / "gates/task"))
        self.assertNotIn("DATABASE_URL", static)
        for cleaned in (task, static):
            for key in (secret, "TGM_TRUSTED_OWNER_DB_URL", "TGM_TASK_SCHEMA_OWNER_DATABASE_URL"):
                self.assertNotIn(key, cleaned)
        self.env["GATES_SCRIPT"] = "exit 7"
        self.bash(source, "if run_gates; then exit 0; else exit $?; fi", expected=7)
        self.env["GATES_SCRIPT"] = "exec /usr/bin/python3 -c " + shlex.quote(probe)
        output = self.bash(source, "if run_gates; then :; else exit $?; fi").stdout
        actual = json.loads(next(line for line in output.splitlines() if line.startswith("{")))
        self.assertEqual(actual["cwd"], str(self.repo))
        self.assertEqual(actual["env"]["HOME"], str(self.home))
        for key, expected in {
            "TGM_GATES_ROOT": str(self.repo), "TGM_TRUSTED_ROOT": str(self.repo),
            "TGM_TRUSTED_TARGET_DIR": self.env["TRUSTED_TARGET_DIR"],
            "TGM_TASK_TARGET_DIR": str(self.root / "gates/task"),
            "TGM_TRUSTED_OWNER_DB_URL": self.env["DRIVER_OWNER_DB_URL"],
            "TGM_TEST_DATABASE_URL": self.env["TASK_DATABASE_URL"],
            "TGM_TASK_SCHEMA_DATABASE_URL": self.env["TASK_SCHEMA_DATABASE_URL"],
        }.items():
            self.assertEqual(actual["env"][key], expected)
        for key in (secret, "DATABASE_URL", "TGM_DB_URL_OWNER"):
            self.assertNotIn(key, actual["env"])

    @unittest.skipUnless(FRESH_DB, "使用 --fresh-db 验证隔离空库")
    def test_fresh_database_migrates_before_dependent_tests(self):
        postgres = "ai-regression-pg-" + uuid.uuid4().hex
        bouncer = "ai-regression-bouncer-" + uuid.uuid4().hex
        for container in (postgres, bouncer):
            self.addCleanup(subprocess.run, ["docker", "rm", "-f", container],
                            capture_output=True)
        subprocess.run([
            "docker", "run", "-d", "--name", postgres,
            "-e", "POSTGRES_PASSWORD=pw", "-e", "POSTGRES_DB=tgm",
            "-v", f"{ROOT / 'scripts/init'}:/docker-entrypoint-initdb.d:ro",
            "-v", f"{ROOT}:/workspace:ro", "postgres:17", "-p", "55432",
        ], check=True, capture_output=True)
        subprocess.run([
            "docker", "exec", postgres, "/bin/bash", "-c",
            "for attempt in {1..60}; do psql -h 127.0.0.1 -p 55432 -U postgres -d tgm "
            "-Atqc 'SELECT 1' && exit 0; sleep 1; done; exit 1",
        ], env={**os.environ, "PGPASSWORD": "pw"}, check=True, capture_output=True)
        subprocess.run([
            "docker", "run", "-d", "--name", bouncer, "--network", f"container:{postgres}",
            "-e", "DB_HOST=127.0.0.1", "-e", "DB_PORT=55432", "-e", "DB_NAME=tgm",
            "-e", "LISTEN_PORT=56432", "-e", "POOL_MODE=transaction",
            "-e", "SERVER_RESET_QUERY_ALWAYS=1", "-e", "MAX_PREPARED_STATEMENTS=200",
            "-e", "AUTH_TYPE=scram-sha-256", "-e", "AUTH_FILE=/etc/pgbouncer/userlist.txt",
            "-v", f"{ROOT / 'scripts/pgbouncer/userlist.txt'}:/etc/pgbouncer/userlist.txt:ro",
            "edoburu/pgbouncer:v1.25.2-p0",
        ], check=True, capture_output=True)
        env = {**self.env, "HOME": os.environ["HOME"], "SQLX_OFFLINE": "true",
               "CARGO_TARGET_DIR": str(ROOT / "target/ai-gates-task")}
        compiled = subprocess.run(
            ["cargo", "test", "--workspace", "--no-run", "--message-format=json"],
            cwd=ROOT, env=env, text=True, capture_output=True, check=True,
        )
        binaries = [Path(event["executable"]).relative_to(ROOT)
                    for line in compiled.stdout.splitlines()
                    if (event := json.loads(line)).get("executable") and event.get("profile", {}).get("test")]
        self.assertTrue(binaries)
        plan = GATES.read_text().split("run_gate static fmt", 1)[1].split("echo \"── 跳过 openapi", 1)[0]
        plan = "run_gate static fmt" + plan
        plan = "\n".join(line for line in plan.splitlines() if
                         line.startswith(("run_gate trusted migrate cargo", "run_gate task test ")))
        body = definitions(GATES.read_text(), "run_gate") + "\n" + "\n".join([
            "MODE=all; started=1; NO_DB=0; FROM=''",
            'run_trusted() { /workspace/target/ai-gates-trusted/debug/tgm migrate; }',
            'run_task() { '
            + "; ".join(shlex.quote("/workspace/" + str(binary)) for binary in binaries)
            + '; }',
            "[ \"$(psql -Atqc \"SELECT to_regclass('public.tenants') IS NULL\")\" = t ]",
            "for attempt in {1..30}; do PGPASSWORD=apw psql -h 127.0.0.1 -p 56432 "
            "-U app_user -d tgm -Atqc 'SELECT 1' && break; sleep 1; done",
            plan,
        ])
        result = subprocess.run([
            "docker", "exec", "-i", "-e", "PGPASSWORD=pw", "-e", "PGUSER=postgres",
            "-e", "PGDATABASE=tgm", "-e", "PGHOST=127.0.0.1", "-e", "PGPORT=55432",
            "-e", "DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control",
            "-e", "TGM_DB_URL_OWNER=postgres://postgres:pw@127.0.0.1:55432/tgm",
            "-e", "SQLX_OFFLINE=true", postgres, "/bin/bash", "-seu", "-o", "pipefail",
        ], input=body, text=True, capture_output=True, timeout=180)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("test result: ok.", result.stdout)
        self.assertLess(result.stdout.index("── 卡口 migrate"), result.stdout.index("── 卡口 test"))
        print(result.stdout, flush=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
