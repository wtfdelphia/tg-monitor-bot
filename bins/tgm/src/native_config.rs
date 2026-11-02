use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::Command;
use std::str::FromStr;
use tgm_core::config;
use tgm_core::profile::Profile;
use zeroize::Zeroizing;

const MAX_FILE_BYTES: u64 = 65_536;

pub fn run(role: &super::Role, path: &Path, check_only: bool) -> Result<()> {
    let identity = fs::metadata("/proc/self").context("无法检查本机运行身份")?;
    ensure!(
        check_only || identity.uid() != 0,
        "应用服务不得以 root 运行"
    );
    let values = read(path, identity.uid(), identity.gid())?;
    validate(config::Role::from(role), &values)?;
    if check_only {
        println!(
            "tgm: {} 原生环境文件离线校验通过；未连接外部服务",
            role.as_str()
        );
        return Ok(());
    }
    let home = format!("/nonexistent/tgm-{}", role.as_str());
    ensure!(
        fs::symlink_metadata(&home)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "应用角色 HOME 必须不存在，避免读取默认 AWS 凭据"
    );
    let executable = std::env::current_exe().context("无法确定当前 tgm 二进制")?;
    let error = Command::new(executable)
        .args(["serve", "--role", role.as_str()])
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", home)
        .envs(values.iter().map(|(name, value)| (name, value.as_str())))
        .exec();
    Err(error).context("无法启动已校验的应用角色")
}

fn read(path: &Path, uid: u32, gid: u32) -> Result<BTreeMap<String, Zeroizing<String>>> {
    ensure!(path.is_absolute(), "环境文件必须使用绝对路径");
    ensure!(
        !path
            .components()
            .any(|component| component == Component::ParentDir),
        "环境文件路径不得含上级目录跳转"
    );
    for parent in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(parent).context("无法检查环境文件父目录")?;
        ensure!(
            metadata.is_dir(),
            "环境文件父目录必须是真实目录，不接受符号链接"
        );
        ensure!(
            metadata.uid() == 0 || metadata.uid() == uid,
            "环境文件父目录属主不可信"
        );
        ensure!(
            metadata.mode() & 0o022 == 0 || (metadata.uid() == 0 && metadata.mode() & 0o1000 != 0),
            "环境文件父目录允许其他身份写入"
        );
    }
    let before = fs::symlink_metadata(path).context("环境文件不存在或无法读取")?;
    ensure!(before.is_file(), "环境文件必须是普通文件，不接受符号链接");
    let mut file = File::open(path).context("无法打开环境文件")?;
    let metadata = file.metadata().context("无法检查环境文件权限")?;
    ensure!(
        metadata.is_file() && metadata.dev() == before.dev() && metadata.ino() == before.ino(),
        "环境文件在读取前被替换"
    );
    let mode = metadata.mode() & 0o7777;
    ensure!(
        (metadata.uid() == uid && mode == 0o600)
            || (metadata.uid() == 0 && metadata.gid() == gid && mode == 0o640),
        "环境文件必须由当前用户所有且权限为 0600，或由 root 所有、服务组所有且权限为 0640"
    );
    ensure!(metadata.len() <= MAX_FILE_BYTES, "环境文件超过大小限制");
    let mut text = Zeroizing::new(String::new());
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| anyhow::anyhow!("环境文件不是可读取的 UTF-8 文本"))?;
    ensure!(text.len() as u64 <= MAX_FILE_BYTES, "环境文件超过大小限制");
    let after = file.metadata().context("无法复查环境文件")?;
    ensure!(
        metadata.uid() == after.uid()
            && metadata.gid() == after.gid()
            && metadata.mode() == after.mode()
            && metadata.len() == after.len()
            && metadata.ctime() == after.ctime()
            && metadata.ctime_nsec() == after.ctime_nsec()
            && metadata.mtime() == after.mtime()
            && metadata.mtime_nsec() == after.mtime_nsec(),
        "环境文件在读取过程中被修改"
    );
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("环境文件第 {} 行不是 NAME=value", index + 1))?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
                && name.as_bytes()[0].is_ascii_uppercase()
                && !value.contains('\0'),
            "环境文件第 {} 行格式不合法",
            index + 1
        );
        ensure!(
            values
                .insert(name.to_owned(), Zeroizing::new(value.to_owned()))
                .is_none(),
            "环境文件第 {} 行重复定义变量",
            index + 1
        );
    }
    Ok(values)
}

fn validate(role: config::Role, values: &BTreeMap<String, Zeroizing<String>>) -> Result<()> {
    let allowed = config::allowed(role);
    ensure!(
        values.keys().all(|name| allowed.contains(&name.as_str())),
        "环境文件包含该角色不允许的变量；变量值不会输出"
    );
    ensure!(
        values
            .values()
            .all(|value| !value.contains("REPLACE_FROM_SECRET")),
        "环境文件仍含发布包示例占位符"
    );
    let get = |name: &str| {
        values
            .get(name)
            .map(|value| value.as_str())
            .filter(|value| !value.is_empty())
    };
    let present: Vec<&str> = values
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, _)| name.as_str())
        .collect();
    let mut problems = config::check(role, get("TGM_PROFILE"), &present, get("RUST_LOG"));
    problems.extend(config::check_kek_backend(
        role,
        get("TGM_KEK_BACKEND"),
        get("TGM_KMS_KEY_ARN"),
        get("TGM_LOCAL_KEK"),
        get("AWS_EC2_METADATA_DISABLED"),
    ));
    ensure!(
        problems.is_empty(),
        "环境文件有 {} 项角色配置错误；按 eng/01 检查，变量值不会输出",
        problems.len()
    );
    if let Some(algorithm) = get("TGM_OIDC_ALG") {
        ensure!(
            config::validate_oidc_alg(algorithm).is_ok(),
            "环境文件的 OIDC 算法不合法"
        );
    }
    if get("TGM_KEK_BACKEND") == Some("env") {
        let profile = Profile::parse(get("TGM_PROFILE").unwrap_or_default())
            .map_err(|_| anyhow::anyhow!("环境文件的运行档位不合法"))?;
        ensure!(
            tgm_core::kek::EnvKek::new(profile, get("TGM_LOCAL_KEK").unwrap_or_default()).is_ok(),
            "环境文件的本地 KEK 配置不合法"
        );
    }
    if let Some(arn) = get("TGM_KMS_KEY_ARN") {
        ensure!(
            super::kms_kek::key_arn_parts(arn).is_some(),
            "环境文件的 KMS key ARN 不合法"
        );
    }
    for (name, value) in values {
        if name.starts_with("TGM_DB_URL_") && !value.is_empty() {
            ensure!(
                sqlx::postgres::PgConnectOptions::from_str(value).is_ok(),
                "环境文件的数据库连接格式不合法；变量值不会输出"
            );
        }
    }
    for name in ["TGM_LISTEN_ADDR", "TGM_BOT_GATEWAY_LISTEN_ADDR"] {
        if let Some(value) = get(name) {
            ensure!(
                value.parse::<std::net::SocketAddr>().is_ok(),
                "环境文件的监听地址不合法"
            );
        }
    }
    for name in [
        "TGM_CONTROL_BOT_IDENTITY_ID",
        "TGM_DATA_BOT_IDENTITY_ID",
        "TGM_DATA_BOT_TENANT_ID",
        "TGM_OIDC_BOT_ID",
    ] {
        if let Some(value) = get(name) {
            ensure!(
                value.parse::<i64>().is_ok_and(|number| number > 0),
                "环境文件的身份编号必须为正整数"
            );
        }
    }
    if let Some(value) = get("TGM_PLATFORM_API_ID") {
        ensure!(
            value.parse::<i32>().is_ok_and(|number| number > 0),
            "环境文件的平台 API 编号必须为正整数"
        );
    }
    for name in [
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    ] {
        if let Some(value) = get(name) {
            ensure!(
                Path::new(value).is_absolute(),
                "AWS 凭据文件必须使用绝对路径"
            );
        }
    }
    Ok(())
}
