use anyhow::{Result, ensure};
use sqlx::PgPool;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{Instant, sleep};

const ROLES: [&str; 6] = [
    "control-plane",
    "bot-gateway",
    "mtproto-worker",
    "fanout",
    "rule",
    "delivery",
];
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct EnvironmentFile {
    directory: PathBuf,
    path: PathBuf,
}

impl EnvironmentFile {
    fn new(contents: &str) -> Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "tgm-native-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("role.env");
        fs::write(&path, contents)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { directory, path })
    }

    fn check(&self, role: &str) -> Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_tgm"))
            .args(["serve", "--role", role, "--env-file"])
            .arg(&self.path)
            .arg("--check-config")
            .env_clear()
            .env("TGM_DB_URL_OWNER", "inherited-private-credential")
            .env("AWS_ACCESS_KEY_ID", "inherited-private-credential")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/tgm-native-test-bus",
            )
            .output()?)
    }
}

impl Drop for EnvironmentFile {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.directory);
    }
}

struct NativeProcess {
    child: Child,
    log: PathBuf,
}

impl NativeProcess {
    fn start(role: &str, environment: &EnvironmentFile) -> Result<Self> {
        let log = environment.directory.join("role.log");
        let output = fs::File::create(&log)?;
        let child = Command::new(env!("CARGO_BIN_EXE_tgm"))
            .args(["serve", "--role", role, "--env-file"])
            .arg(&environment.path)
            .env_clear()
            .env("TGM_DB_URL_OWNER", "inherited-private-credential")
            .env("AWS_ACCESS_KEY_ID", "inherited-private-credential")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(Self { child, log })
    }

    async fn ready(&mut self, role: &str) -> Result<()> {
        let marker = match role {
            "control-plane" | "bot-gateway" => "HTTP 服务已启动",
            "mtproto-worker" | "delivery" => "已启动",
            _ => "subscriber 已装",
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "{role} 提前退出 {status}：{}",
                    fs::read_to_string(&self.log)?
                );
            }
            if fs::read_to_string(&self.log)?.contains(marker) {
                sleep(Duration::from_millis(100)).await;
                ensure!(self.child.try_wait()?.is_none(), "{role} 未保持运行");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "{role} 在 30s 内未启动");
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(&mut self, role: &str) -> Result<()> {
        ensure!(
            Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()?
                .success(),
            "SIGTERM 未发送成功"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(
                    status.success(),
                    "{role} 未优雅退出 {status}：{}",
                    fs::read_to_string(&self.log)?
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "{role} 收到 SIGTERM 后 30s 内未退出"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        let _result = self.child.kill();
        let _result = self.child.wait();
    }
}

fn assert_private(output: &Output) {
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-credential"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-credential"));
}

fn role_example(role: &str) -> Result<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/app");
    let example = fs::read_to_string(root.join(format!("{role}.env.example")))?;
    Ok(example
        .lines()
        .map(|line| {
            let (name, value) = line.split_once('=').unwrap_or((line, ""));
            let replacement = match name {
                "TGM_KMS_KEY_ARN" => {
                    "arn:aws:kms:ap-southeast-2:123456789012:key/test-key".to_owned()
                }
                "AWS_CONFIG_FILE" => "/nonexistent/tgm-test-aws-config".to_owned(),
                "TGM_CONTROL_BOT_IDENTITY_ID"
                | "TGM_DATA_BOT_IDENTITY_ID"
                | "TGM_OIDC_BOT_ID"
                | "TGM_PLATFORM_API_ID" => "1".to_owned(),
                _ => value.replace("REPLACE_FROM_SECRET", "private-credential"),
            };
            format!("{name}={replacement}\n")
        })
        .collect())
}

#[test]
fn native_files_check_all_roles_without_external_services() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/app");
    for role in ROLES {
        let example = fs::read_to_string(root.join(format!("{role}.env.example")))?;
        let valid = role_example(role)?;
        let environment = EnvironmentFile::new(&valid)?;
        let output = environment.check(role)?;
        ensure!(
            output.status.success(),
            "{role}：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_private(&output);
        assert!(environment.check("unknown-role")?.status.code() == Some(2));
        assert!(
            !EnvironmentFile::new(&example)?
                .check(role)?
                .status
                .success()
        );
        for line in valid.lines() {
            let name = line
                .split_once('=')
                .map(|(name, _)| name)
                .unwrap_or_default();
            if !name.starts_with("TGM_")
                || matches!(
                    name,
                    "TGM_LISTEN_ADDR" | "TGM_BOT_GATEWAY_LISTEN_ADDR" | "TGM_DB_URL_EXECUTION"
                )
            {
                continue;
            }
            let missing: String = valid
                .lines()
                .filter(|candidate| *candidate != line)
                .map(|candidate| format!("{candidate}\n"))
                .collect();
            let output = EnvironmentFile::new(&missing)?.check(role)?;
            assert!(!output.status.success(), "{role} 缺少 {name} 未被拒绝");
            assert_private(&output);
        }
        for name in [
            "TGM_DB_URL_OWNER",
            "TGM_DB_URL_AUTH_CLEANUP",
            "TGM_DB_URL_TENANT_PURGE",
            "DATABASE_URL",
            "LD_PRELOAD",
            "UNREGISTERED_SECRET",
        ] {
            for value in ["private-credential", ""] {
                let output =
                    EnvironmentFile::new(&format!("{valid}{name}={value}\n"))?.check(role)?;
                assert!(!output.status.success(), "{role} 多配 {name} 未被拒绝");
                assert_private(&output);
            }
        }
    }
    Ok(())
}

#[test]
fn native_files_reject_foreign_empty_and_unknown_variables() -> Result<()> {
    let valid = "TGM_PROFILE=development\nTGM_DB_URL_APP=postgres://app_user:private-credential@127.0.0.1:1/tgm\nTGM_REDIS_URL=redis://127.0.0.1:1\n";
    for name in [
        "TGM_DB_URL_OWNER",
        "TGM_DB_URL_AUTH_CLEANUP",
        "TGM_DB_URL_TENANT_PURGE",
        "TGM_PLATFORM_API_HASH",
        "AWS_ACCESS_KEY_ID",
        "DATABASE_URL",
        "LD_PRELOAD",
        "PATH",
        "UNREGISTERED_SECRET",
    ] {
        for value in ["private-credential", ""] {
            let environment = EnvironmentFile::new(&format!("{valid}{name}={value}\n"))?;
            let output = environment.check("rule")?;
            assert!(!output.status.success(), "{name} 未被拒绝");
            assert_private(&output);
        }
    }
    for invalid in [
        "TGM_PROFILE=development\n",
        "TGM_PROFILE=private-credential\n",
        "export TGM_PROFILE=private-credential\n",
        "TGM_PROFILE=development\nTGM_PROFILE=private-credential\n",
        "TGM_PROFILE=development\nINVALID\n",
        "TGM_PROFILE=development\nTGM_REDIS_URL=private-credential\0\n",
    ] {
        let output = EnvironmentFile::new(invalid)?.check("rule")?;
        assert!(!output.status.success());
        assert_private(&output);
    }
    Ok(())
}

#[test]
fn native_files_reject_missing_symlink_and_wide_permissions() -> Result<()> {
    let valid = "TGM_PROFILE=development\nTGM_DB_URL_APP=postgres://app_user:private-credential@127.0.0.1:1/tgm\nTGM_REDIS_URL=redis://127.0.0.1:1\n";
    let mut environment = EnvironmentFile::new(valid)?;
    for mode in [0o644, 0o660, 0o400, 0o1600] {
        fs::set_permissions(&environment.path, fs::Permissions::from_mode(mode))?;
        let output = environment.check("rule")?;
        assert!(!output.status.success(), "权限 {mode:o} 未被拒绝");
        assert_private(&output);
    }
    fs::set_permissions(&environment.path, fs::Permissions::from_mode(0o600))?;
    assert!(environment.check("rule")?.status.success());
    let link = environment.directory.join("symlink.env");
    symlink(&environment.path, &link)?;
    environment.path = link;
    assert!(!environment.check("rule")?.status.success());
    environment.path = environment.directory.join("missing.env");
    assert!(!environment.check("rule")?.status.success());
    environment.path = environment.directory.join("role.env");
    fs::set_permissions(&environment.directory, fs::Permissions::from_mode(0o777))?;
    assert!(!environment.check("rule")?.status.success());
    fs::set_permissions(&environment.directory, fs::Permissions::from_mode(0o700))?;
    let parent_link = environment.directory.join("parent-link");
    symlink(&environment.directory, &parent_link)?;
    environment.path = parent_link.join("role.env");
    assert!(!environment.check("rule")?.status.success());
    Ok(())
}

#[test]
fn native_files_reject_bad_values_sizes_and_relative_paths() -> Result<()> {
    for (name, value) in [
        ("TGM_PROFILE", "private-credential"),
        ("TGM_DB_URL_APP", "private-credential"),
        ("TGM_KMS_KEY_ARN", "private-credential"),
        ("TGM_OIDC_ALG", "private-credential"),
        ("TGM_CONTROL_BOT_IDENTITY_ID", "private-credential"),
        ("TGM_LISTEN_ADDR", "private-credential"),
        ("AWS_CONFIG_FILE", "relative/private-credential"),
        ("AWS_EC2_METADATA_DISABLED", "false"),
        ("RUST_LOG", "private-credential"),
    ] {
        let contents: String = role_example("control-plane")?
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{name}=")) {
                    format!("{name}={value}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        let output = EnvironmentFile::new(&contents)?.check("control-plane")?;
        assert!(!output.status.success(), "{name} 的错误值未被拒绝");
        assert_private(&output);
    }
    let mut environment = EnvironmentFile::new(&format!("#{}", "x".repeat(65_536)))?;
    assert!(!environment.check("rule")?.status.success());
    fs::write(&environment.path, [0xff, 0xfe])?;
    assert!(!environment.check("rule")?.status.success());
    environment.path = PathBuf::from("relative.env");
    assert!(!environment.check("rule")?.status.success());
    Ok(())
}

#[test]
fn native_files_do_not_source_shell_or_inherit_process_credentials() -> Result<()> {
    let marker =
        std::env::temp_dir().join(format!("tgm-native-shell-marker-{}", std::process::id()));
    let environment = EnvironmentFile::new(&format!(
        "TGM_PROFILE=development\nTGM_DB_URL_APP=postgres://app_user:private-credential@127.0.0.1:1/tgm\nTGM_REDIS_URL=$(touch {})\n",
        marker.display()
    ))?;
    assert!(environment.check("rule")?.status.success());
    assert!(!marker.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_tgm"))
        .args(["serve", "--role", "rule", "--env-file"])
        .arg(&environment.path)
        .env("TGM_DB_URL_OWNER", "inherited-private-credential")
        .output()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("TGM_DB_URL_OWNER"));
    assert_private(&output);
    Ok(())
}

#[test]
fn native_units_keep_single_role_identity_and_bounded_restart() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/app");
    for relative in ["tgm-app@.service", "user/tgm-app@.service"] {
        let unit = fs::read_to_string(root.join(relative))?;
        for required in [
            "Type=simple",
            "serve --role %i --env-file",
            "--check-config",
            "/usr/bin/env -i",
            "Restart=on-failure",
            "RestartSec=10s",
            "StartLimitInterval=120s",
            "StartLimitBurst=3",
            "KillSignal=SIGTERM",
            "KillMode=control-group",
            "TimeoutStopSec=30s",
            "UMask=0077",
            "StandardOutput=journal",
            "StandardError=journal",
            "NoNewPrivileges=true",
        ] {
            assert!(unit.contains(required), "{relative} 缺少 {required}");
        }
        assert!(!unit.contains("ExecStop="));
        assert!(!unit.contains("EnvironmentFile="));
        assert!(!unit.contains("migrate"));
        assert!(!unit.contains("tgm-cleanup"));
        let unit_section = unit
            .split_once("[Unit]")
            .ok_or_else(|| anyhow::anyhow!("{relative} 缺少 Unit 段"))?
            .1
            .split("[Service]")
            .next()
            .unwrap_or_default();
        assert!(unit_section.contains("StartLimitInterval=120s"));
        assert!(unit_section.contains("StartLimitBurst=3"));
    }
    let system = fs::read_to_string(root.join("tgm-app@.service"))?;
    assert!(system.contains("User=tgm-%i\nGroup=tgm-%i"));
    assert!(system.contains("ProtectSystem=full"));
    assert!(!system.contains("ProtectSystem=strict"));
    let user = fs::read_to_string(root.join("user/tgm-app@.service"))?;
    assert!(!user.contains("\nUser="));
    assert!(!user.contains("ProtectHome="));
    assert!(!user.contains("PrivateTmp="));
    assert!(user.contains("same UID"));
    Ok(())
}

#[test]
fn native_installer_and_archive_regressions() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .arg(root.join("scripts/check-native-services.py"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()?;
    ensure!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn native_roles_start_with_restricted_credentials_and_stop_cleanly(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let platform_identity: i64 = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', NULL, 'offline') RETURNING id",
    ).fetch_one(&admin).await?;
    for role in ROLES {
        let app_url = format!(
            "postgres://app_user:apw@127.0.0.1:55432/{database}?application_name=tgm-native-{role}"
        );
        let app = PgPool::connect(&app_url).await?;
        let current_user: String = sqlx::query_scalar("SELECT current_user")
            .fetch_one(&app)
            .await?;
        assert_eq!(current_user, "app_user");
        let mut values = vec![
            ("TGM_PROFILE", "development".to_owned()),
            ("TGM_DB_URL_APP", app_url),
            ("TGM_REDIS_URL", "redis://127.0.0.1:56379".to_owned()),
            ("RUST_LOG", "info,sqlx=warn".to_owned()),
        ];
        if ["control-plane", "bot-gateway", "mtproto-worker", "delivery"].contains(&role) {
            values.extend([
                ("TGM_KEK_BACKEND", "env".to_owned()),
                (
                    "TGM_LOCAL_KEK",
                    "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=".to_owned(),
                ),
            ]);
        }
        if ["control-plane", "bot-gateway"].contains(&role) {
            values.push((
                "TGM_DB_URL_AUTH",
                format!("postgres://auth_lookup:lpw@127.0.0.1:55432/{database}"),
            ));
        }
        if ["fanout", "mtproto-worker", "delivery"].contains(&role) {
            values.extend([
                ("TGM_S3_ENDPOINT", "http://127.0.0.1:59000".to_owned()),
                ("TGM_S3_BUCKET", "tgm-native-systemd".to_owned()),
                ("TGM_S3_ACCESS_KEY_ID", "minioadmin".to_owned()),
                ("TGM_S3_SECRET_ACCESS_KEY", "minioadmin".to_owned()),
            ]);
        }
        if role == "fanout" {
            values.push((
                "TGM_DB_URL_MEDIA_CLEANUP",
                format!("postgres://media_cleanup:mcpw@127.0.0.1:55432/{database}"),
            ));
        }
        if role == "delivery" {
            values.extend([
                ("TGM_PLATFORM_API_ID", "1".to_owned()),
                (
                    "TGM_PLATFORM_API_HASH",
                    "00000000000000000000000000000000".to_owned(),
                ),
            ]);
        }
        if role == "control-plane" {
            values.extend([
                (
                    "TGM_DB_URL_OPS",
                    format!("postgres://platform_ops:ppw@127.0.0.1:55432/{database}"),
                ),
                ("TGM_OIDC_BOT_ID", "1".to_owned()),
                ("TGM_OIDC_ALG", "RS256".to_owned()),
                (
                    "TGM_OIDC_REDIRECT_URL",
                    "http://127.0.0.1/callback".to_owned(),
                ),
                ("TGM_OIDC_CLIENT_SECRET", "private-credential".to_owned()),
                ("TGM_CONTROL_BOT_TOKEN", "1:private-credential".to_owned()),
                (
                    "TGM_CONTROL_BOT_SECRET_TOKEN",
                    "private-credential".to_owned(),
                ),
                ("TGM_CONTROL_BOT_IDENTITY_ID", platform_identity.to_string()),
                ("TGM_CONTROL_BOT_USERNAME", "native_test_bot".to_owned()),
                ("TGM_LISTEN_ADDR", "127.0.0.1:0".to_owned()),
            ]);
        }
        if role == "bot-gateway" {
            values.extend([
                ("TGM_DATA_BOT_SECRET_TOKEN", "private-credential".to_owned()),
                ("TGM_DATA_BOT_IDENTITY_ID", platform_identity.to_string()),
                ("TGM_BOT_GATEWAY_LISTEN_ADDR", "127.0.0.1:0".to_owned()),
            ]);
        }
        let contents: String = values
            .iter()
            .map(|(name, value)| format!("{name}={value}\n"))
            .collect();
        let environment = EnvironmentFile::new(&contents)?;
        ensure!(
            environment.check(role)?.status.success(),
            "{role} 离线检查失败"
        );
        let mut process = NativeProcess::start(role, &environment)?;
        process.ready(role).await?;
        let process_environment = fs::read(format!("/proc/{}/environ", process.child.id()))?;
        let variable_names: Vec<&[u8]> = process_environment
            .split(|byte| *byte == 0)
            .filter_map(|entry| entry.split(|byte| *byte == b'=').next())
            .collect();
        for name in [
            "TGM_DB_URL_OWNER",
            "DATABASE_URL",
            "AWS_ACCESS_KEY_ID",
            "TGM_DB_URL_TENANT_PURGE",
        ] {
            assert!(
                !variable_names.contains(&name.as_bytes()),
                "{role} 继承了 {name}"
            );
        }
        process.stop(role).await?;
        if std::env::var("TGM_NATIVE_SYSTEMD_TESTS").as_deref() == Ok("1") {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let mut command = Command::new("python3");
            command
                .arg(root.join("scripts/check-native-service-runtime.py"))
                .args([
                    "--binary",
                    env!("CARGO_BIN_EXE_tgm"),
                    "--role",
                    role,
                    "--env-file",
                ])
                .arg(&environment.path);
            if std::env::var("TGM_NATIVE_SYSTEMD_ISOLATED_MANAGER").as_deref() == Ok("1") {
                command.arg("--isolated-manager");
            }
            let output = command.output()?;
            ensure!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
        let logs = fs::read_to_string(&process.log)?;
        for secret in [
            "private-credential",
            "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
            "postgres://app_user:apw",
            "minioadmin",
        ] {
            assert!(!logs.contains(secret), "{role} 日志泄漏凭据");
        }
        app.close().await;
    }
    Ok(())
}
