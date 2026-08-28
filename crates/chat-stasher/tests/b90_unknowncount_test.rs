//! B90 — 「数不出来」被印成了一个具体的数字。
//!
//! 这个仓最硬的规矩是「不能把未知当成空」。B90 收的是它的**计数**变体：
//! 一个数不出来的量，落到屏幕上时变成了 `0` / `0 天`，读的人无法把它和
//! 「真的是零」区分开。本文件收三处里能端到端复现的两处：
//!
//!   * **B** —— `doctor` 的 Claude Code 风险行。`earliest` 是 `None` 时日期
//!     诚实地印 `n/a`，天数却偷偷变成 `0.0`，于是同一句话里一半诚实一半是编的，
//!     而且编出来的那一半是**让人立刻采取行动的假警报**：「你的历史只还剩约 0 天」。
//!   * **C** —— `status --sessions` 的 mtime 列。取不到 / 早于纪元的 mtime
//!     打成 `0`，与「真的等于 1970-01-01」不可分。这正是 `inbox.rs` 刚修掉的
//!     那个 bug，只是没扫到这张表。
//!
//! （A —— `sqlite_probe::unreadable_candidate_count` 自己读不出来时报 `0` ——
//! 需要让「第二次打开同一个库」失败，无法在一个进程里确定性地端到端复现，
//! 反证测试放在 `src/sqlite_probe.rs` 与 `src/doctor.rs` 的单元测试里。）
//!
//! 全部只在 `tempfile` 临时目录里跑：HOME / XDG_* / CHAT_STASHER_REGISTRY /
//! CURSOR_USER_DIR 全部改道进沙箱，绝不碰真实 harness 目录，也绝不读会话正文。

use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// `doctor::run()` 走的是进程级环境变量，而 cargo 在同一进程里并行跑测试。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn isolate_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    std::env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
    std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
    for var in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
        "CHAT_STASHER_REGISTRY",
    ] {
        std::env::remove_var(var);
    }
}

// ---------------------------------------------------------------------------
// B —— 同一句话里一半诚实一半编的
// ---------------------------------------------------------------------------

/// 取出 Claude Code 那条风险行。
fn claude_risk(report: &chat_stasher::doctor::DoctorReport) -> String {
    report
        .risks
        .iter()
        .find(|line| line.contains("Claude Code"))
        .cloned()
        .unwrap_or_else(|| panic!("doctor 没有输出任何 Claude Code 风险行：{:?}", report.risks))
}

/// **B 的反证。** 一台没有任何 Claude 会话的机器上：`earliest` 是 `None`，
/// 于是「最早的会话是 n/a」是诚实的，「约 0 天前」「只还剩约 0 天」是编的。
/// 改之前这个断言必须红——它断言的正是那个编出来的数字不许出现。
#[test]
fn claude_risk_never_invents_a_day_count_it_does_not_have() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        !line.contains("0 days ago"),
        "最早会话未知时不许印出「约 0 天前」这个编出来的数字；实际输出：\n{line}"
    );
    assert!(
        !line.contains("only about 0 days left"),
        "「只还剩约 0 天」是一句让人立刻采取行动的假警报；实际输出：\n{line}"
    );
}

/// 编出来的数字被拿掉之后，这句话不许退化成沉默：风险本身（cleanupPeriodDays
/// 未设置 → 默认 30 天）仍然存在，只是天数说成「未知」。
#[test]
fn claude_risk_still_says_the_retention_risk_and_names_the_unknown() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        line.contains("cleanupPeriodDays is unset"),
        "风险本身没有消失，仍然要说；实际输出：\n{line}"
    );
    assert!(
        line.contains("unknown"),
        "说不出天数就要明说「unknown」，而不是不吭声；实际输出：\n{line}"
    );
}

/// 体面的失败路径：一台**有**会话的机器上，这行还是要给出真实日期和天数。
/// 这条同时是「健康机器上它不吵」的证据——未知分支没有污染已知分支。
#[test]
fn a_machine_with_sessions_still_gets_a_real_date_and_a_real_day_count() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());
    // 一个真实形状的 claude-code 会话文件，mtime 就是现在。
    let projects = sandbox.path().join(".claude").join("projects").join("p");
    fs::create_dir_all(&projects).expect("create claude projects dir");
    fs::write(projects.join("s.jsonl"), "{}\n").expect("write session file");

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        !line.contains("n/a"),
        "有会话时日期必须是真的；实际输出：\n{line}"
    );
    assert!(
        line.contains("about 0 days ago"),
        "有会话时天数必须照旧印出来（刚写的文件就是 0 天前）；实际输出：\n{line}"
    );
}

// ---------------------------------------------------------------------------
// C —— status --sessions 的 mtime 列
// ---------------------------------------------------------------------------

/// 一条 `createdAt` 为负数的 Cursor composer：它的 mtime 早于纪元，
/// `duration_since(UNIX_EPOCH)` 失败，旧代码把它打成 `0`。
const PRE_EPOCH_ROW: &str =
    r#"{"composerId":"a","createdAt":-1000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;
/// 同样合格、但时间戳正常的一条，用来证明正常行的显示没被改坏。
const NORMAL_ROW: &str = r#"{"composerId":"b","createdAt":1760000000000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;

fn plant_rows(user_dir: &Path, rows: &[(&str, &str)]) {
    let db = user_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db.parent().expect("db path has a parent")).expect("create globalStorage");
    let conn = Connection::open(&db).expect("open fixture store");
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .expect("create fixture table");
    for (key, value) in rows {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .expect("insert fixture row");
    }
    drop(conn);
}

/// 一个 harness 的 registry，三个平台槽位都放同一个 Cursor cell，
/// 让 fixture 在任何 OS 上走同一条代码路径。
fn write_cursor_registry(base: &Path) -> PathBuf {
    let cursor = r#"{ "template": "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                      "env_override": "CURSOR_USER_DIR", "format": "sqlite",
                      "confidence": "measured-locally", "source": "B90 test fixture",
                      "sql_table": "cursorDiskKV", "sql_id_column": "key",
                      "sql_required_columns": ["key", "value"],
                      "sql_key_column": "key", "sql_key_pattern": "composerData:%",
                      "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
                      "sql_qualification": "cursor_composer" }"#;
    let path = base.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B90",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cursor}, "linux": {cursor}, "windows": {cursor} }} }}
                  ] }}"#
        ),
    )
    .expect("write fixture registry");
    path
}

/// `status --sessions` 的表体（去掉 `[run-once]` 那行的墙钟内容）。
fn run_status_sessions(sandbox: &Path, rows: &[(&str, &str)]) -> String {
    let home = sandbox.join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).expect("create sandbox home");
    plant_rows(&user_dir, rows);
    let registry = write_cursor_registry(sandbox);

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("status")
        .arg("--sessions")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env("CURSOR_USER_DIR", &user_dir)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run status --sessions");

    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// 表里 mtime 那一列的所有取值。
fn mtime_column(body: &str) -> Vec<String> {
    body.lines()
        .filter(|line| line.trim_start().starts_with("cursor "))
        .filter(|line| !line.contains("sessions :"))
        .filter_map(|line| line.split_whitespace().nth(2).map(str::to_string))
        .collect()
}

/// **C 的反证。** 一条早于纪元的 mtime 在表里被打成 `0`，和「真的等于纪元」
/// 不可分。改之前这个断言必须红。
#[test]
fn status_sessions_never_prints_an_unknown_mtime_as_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let body = run_status_sessions(sandbox.path(), &[("composerData:aaa", PRE_EPOCH_ROW)]);
    let column = mtime_column(&body);

    assert_eq!(
        column.len(),
        1,
        "fixture 应当只产出一行；实际输出：\n{body}"
    );
    assert_ne!(
        column[0], "0",
        "取不到 / 早于纪元的 mtime 不许打成 0——那和「真的等于 1970-01-01」不可分；实际输出：\n{body}"
    );
    assert!(
        column[0].contains("unknown"),
        "它应当显示成「未知」；实际输出：\n{body}"
    );
}

/// 健康机器上「它不响」：时间戳正常的一行照旧印出秒数，一个字都不多。
#[test]
fn a_readable_mtime_still_prints_its_seconds() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let body = run_status_sessions(sandbox.path(), &[("composerData:bbb", NORMAL_ROW)]);
    let column = mtime_column(&body);

    assert_eq!(column, vec!["1760000000".to_string()], "实际输出：\n{body}");
    assert!(
        !body.contains("unknown"),
        "没有未知的东西时，输出里不许出现「unknown」；实际输出：\n{body}"
    );
}
