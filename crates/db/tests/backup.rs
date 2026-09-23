//! R7：受限角色跑 `pg_dump` 必须失败，而不是产出部分数据。
//! 清单在 docs/design/eng/03-测试策略.md §三，定义处是
//! docs/design/spec/05-安全与租户隔离.md §3.5 末段。
//!
//! 单独一个文件而不是并进 isolation.rs：这条是唯一一条要起外部进程的。
//! 它也不能用 `#[sqlx::test]` —— 那个宏建的临时库名字随机，而 pg_dump 要
//! 一个连接串；更要紧的是本条测的是**角色的权限**，与建在哪个库无关。
//! 所以走 compose 那个常驻库，同 pgbouncer.rs 的处置。

// 同 isolation.rs：反向测试里 expect 与 panic 就是断言本身。
#![allow(clippy::expect_used, clippy::panic)]

use std::process::Command;

/// 受限角色的连接串。口令是 eng/02 §二 的本地开发值。
const APP_URL: &str = "postgres://app_user:apw@127.0.0.1:55432/tgm";

/// 找 pg_dump。它不在非交互 shell 的 PATH 里（eng/00 §零 那句「✓ 已装」
/// 对脚本和 CI 都是假的），所以这里按 env.sh 里那个前缀兜一次。
/// 找不到就**响亮地失败** —— 一条静默跳过的测试与一条通过的测试同形。
///
/// **「能起」不够，必须是 17.x。** 原先这里只看 `--version` 能不能执行成功，
/// 于是 CI 首跑挑中了 runner 自带的 16.15（装了 client-17 但 /usr/bin 被 16 占住），
/// pg_dump 对更高版本的服务端直接拒绝。这对反证那条只是报红，
/// 但对 **R7 正向那条是危险的**：它的判据是「退出码非 0 且没吐出数据」，
/// 而版本不匹配恰好满足两者 —— 那条会因为错误的原因变绿。
/// 所以版本在挑选阶段就断言，不留给调用方。
fn pg_dump() -> String {
    let mut tried = Vec::new();
    for cand in ["pg_dump", "/usr/lib/postgresql/17/bin/pg_dump"] {
        match version_of(cand) {
            Some(v) if v.contains("PostgreSQL) 17.") => return cand.into(),
            Some(v) => tried.push(format!("{cand} → {}", v.trim())),
            None => tried.push(format!("{cand} → 起不来")),
        }
    }
    let home = std::env::var("HOME").expect("HOME 未设");
    let p = format!("{home}/opt/pgdg17/usr/lib/postgresql/17/bin/pg_dump");
    match version_of(&p) {
        Some(v) if v.contains("PostgreSQL) 17.") => p,
        other => panic!(
            "找不到 17.x 的 pg_dump —— 这条测试需要它，不能当成通过。\n\
             试过：{}\n{p} → {}\n装法见 docs/design/eng/00-工程约定.md §零",
            tried.join("；"),
            other.as_deref().unwrap_or("不存在或起不来").trim()
        ),
    }
}

fn version_of(bin: &str) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// R7 本体。判据是**两件事同时成立**：退出码非 0，且 stdout 里没有业务数据。
///
/// 只断言退出码是不够的：pg_dump 可以先吐一部分 COPY 数据再失败，
/// 那种失败下「备份任务红了」与「备份不完整」是两回事，而后者更危险 ——
/// 一个红着的任务会被人看见，一份少了几张表的 dump 不会。
///
/// 也**不断言报错文本**。实测拦住它的是 GRANT 层（序列没授权）而不是 RLS 层，
/// 而拦在哪一层会随 GRANT 清单变动 —— 「没漏出数据」不会。详见 spec/05 §3.5。
#[test]
fn r7_restricted_role_cannot_pg_dump() {
    let out = Command::new(pg_dump())
        .args(["--data-only", APP_URL])
        .output()
        .expect("起 pg_dump 失败");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "受限角色竟然 dump 成功了 —— 备份路径绕过了隔离。\nstdout 前 500 字节：{}",
        stdout.chars().take(500).collect::<String>()
    );

    // 第二半判据：失败得干净。COPY 是 --data-only 输出业务数据的形式。
    assert!(
        !stdout.contains("COPY public."),
        "pg_dump 失败前已经吐出了业务数据 —— 那是「备份不完整」而不是「备份失败」：\n{stdout}"
    );
}

/// 反证上一条不是「pg_dump 在这个环境里对谁都失败」。
///
/// 没有这一条，一个连不上库、或者 pg_dump 本身坏掉的环境会让 R7 全绿 ——
/// 而那正是 spec/05 §3.5 末段那句运维前提（备份必须用 owner 跑）要立住的反面。
/// 这一条同时是那句前提的正向证据：owner 跑得通。
#[test]
fn r7_counterproof_superuser_can_pg_dump() {
    let url = std::env::var("TGM_DB_URL_OWNER")
        .unwrap_or_else(|_| "postgres://postgres:pw@127.0.0.1:55432/tgm".into());

    let out = Command::new(pg_dump())
        .args(["--schema-only", &url])
        .output()
        .expect("起 pg_dump 失败");

    assert!(
        out.status.success(),
        "超级用户 dump 也失败了 —— 那么 R7 的红说明不了任何事：\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("CREATE TABLE public.keywords"),
        "dump 成功了但没有表定义 —— 这个反证是空的"
    );
}
