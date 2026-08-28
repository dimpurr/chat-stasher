//! B84 — "`status` says the machine never ran, and exits 0". It does not.
//!
//! The observation that opened this ticket was a real terminal session:
//!
//! ```text
//! [run-once] 还没有任何运行记录：本机从未成功跑完一次 run-once（…）。
//! [scan] 505 个会话（0 compressed）：…
//! ```
//!
//! …reported alongside exit code 0. The code was never 0. `cmd_status` maps
//! `run_state_info(&config).verdict.healthy` straight onto `SUCCESS` /
//! `FAILURE`, and `runstate::summarize` has returned `healthy: false` for
//! `RunStateRead::Missing` since the file was written (`runstate.rs:186-192`).
//!
//! What produces a 0 in a transcript is the *shell*, not the binary: the whole
//! report goes to stderr, so it is normally read through `2>&1 | head`, and a
//! pipeline's status is the last command's. Measured on this checkout, same
//! sandbox, same binary:
//!
//! ```text
//! $ chat-stasher status                    ; echo $?   ->  1
//! $ chat-stasher status 2>&1 | head -20    ; echo $?   ->  0
//! $ chat-stasher status 2>&1 | cat         ; echo $?   ->  0
//! ```
//!
//! So this file changes no behaviour. It pins the behaviour that already
//! exists, from the angle the ticket asked about — is "never ran" *meant* to be
//! non-zero — so that a future "surely a fresh install isn't unhealthy" patch
//! has to delete an assertion rather than quietly flip the code. The reasoning
//! lives next to the code it governs, in the B84 comment in `cmd_status`.
//!
//! The exit-code contract these assertions are written against
//! (`b80_exitfamily_test.rs`, `cmd_search` / `cmd_collect` comments):
//!
//! ```text
//! 3 = 没读完 / 根本没读      1 = 读完了失败      2 = 用法错      0 = 干净
//! ```
//!
//! Everything below runs the real binary inside a `tempfile` sandbox with
//! `HOME` / `USERPROFILE` / `XDG_*` redirected into it. No real harness
//! directory, state dir, stage, archive or remote is touched, and no session
//! body is read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FINISHED_AND_FAILED: i32 = 1;
const CLEAN: i32 = 0;

/// The sentence `runstate::summarize` returns for `RunStateRead::Missing`,
/// quoted so a reworded verdict cannot silently detach these assertions from
/// the case they are about.
const NEVER_RAN: &str = "run-once has never completed successfully";

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path) -> std::process::Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create sandbox home");
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("status")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env_remove("CHAT_STASHER_REGISTRY")
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .output()
        .expect("run chat-stasher status")
}

/// Where `collect::default_state_dir()` lands given the `XDG_DATA_HOME` above.
fn state_dir(sandbox: &Path) -> PathBuf {
    sandbox.join("xdg-data").join("chat-stasher").join("state")
}

/// Write a `run-state.json` describing a pass that ended a minute ago.
/// `outcome` is the serde kebab-case wire form (`noop` / `completed` / `error`).
fn write_run_state(sandbox: &Path, outcome: &str, failed_step: &str) {
    let dir = state_dir(sandbox);
    fs::create_dir_all(&dir).expect("create sandbox state dir");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    let body = format!(
        r#"{{ "version": 1, "finished_at_unix": {}, "duration_ms": 42,
              "outcome": "{outcome}", "failed_step": {failed_step},
              "shards_written": 0, "stage_shards": 0, "snapshot_created": false,
              "collect_errors": 0, "archive_gaps": 0,
              "machine_digest": "aaaaaaaaaaaa" }}"#,
        now.saturating_sub(60)
    );
    fs::write(dir.join("run-state.json"), body).expect("write run-state fixture");
}

fn stdio(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Everything except the `[run-once]` verdict line — the part of the report
/// that must not move when only the run-state fixture changes.
fn body_without_verdict(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("[run-once]"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The ticket's question, answered as an assertion
// ---------------------------------------------------------------------------

/// A machine with no `run-state.json` — a fresh install, or a state dir that
/// was wiped — is **not** reported as clean. This is the answer to "is a
/// never-run machine a normal new-install state or a problem the user should
/// know about": for the exit code, it is a problem, because that integer is
/// the only channel a script has and an absent record is the absence of
/// evidence, not evidence of health (`runstate.rs:186-192`,
/// `docs/install.md`).
#[test]
fn never_ran_exits_non_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let out = run(sandbox.path());
    let text = stdio(&out);

    assert!(
        text.contains(NEVER_RAN),
        "fixture 必须真的走到「从未跑过」那条判定；实际输出：\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(FINISHED_AND_FAILED),
        "扫描读完了、定时器判定为不健康 = 1；把它改成 0 会让「定时器从没装」和\
         「定时器装了但死了」在脚本眼里长得一样；实际输出：\n{text}"
    );
}

/// …and it is **1**, not 3: the scan finished, it is the verdict that is bad.
/// 3 is reserved for "did not read / did not finish" (`cmd_status`'s
/// `scanner::scan` error arm, B80).
#[test]
fn never_ran_is_the_finished_and_failed_code_not_the_did_not_read_code() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let out = run(sandbox.path());
    let text = stdio(&out);

    assert!(
        !text.contains("status: scan failed"),
        "这条 fixture 必须是「扫描成功了」的那条路径；实际输出：\n{text}"
    );
}

/// The guard against "return 1 unconditionally": a machine whose last pass
/// succeeded a minute ago is clean. Without this, the assertion above could be
/// satisfied by a `status` that never exits 0 at all.
#[test]
fn a_recent_successful_run_still_exits_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    write_run_state(sandbox.path(), "noop", "null");
    let out = run(sandbox.path());
    let text = stdio(&out);

    assert_eq!(
        out.status.code(),
        Some(CLEAN),
        "上次运行成功且不过期 = 0；实际输出：\n{text}"
    );
}

/// The other non-zero verdict, kept here so the three run-state cases sit
/// together: a recorded failure is also 1.
#[test]
fn a_recorded_failure_exits_non_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    write_run_state(sandbox.path(), "error", r#""push""#);
    let out = run(sandbox.path());
    let text = stdio(&out);

    assert_eq!(
        out.status.code(),
        Some(FINISHED_AND_FAILED),
        "上次运行失败 = 1；实际输出：\n{text}"
    );
}

/// The exit code moves with the `[run-once]` line and with nothing else: the
/// scan half of the report is byte-identical between the never-ran machine and
/// the healthy one. This is what makes the code readable as a verdict about
/// the *scheduler* rather than about the report as a whole.
#[test]
fn only_the_verdict_line_differs_between_the_two_exit_codes() {
    let never = tempfile::tempdir().expect("sandbox");
    let healthy = tempfile::tempdir().expect("sandbox");
    write_run_state(healthy.path(), "noop", "null");

    let never_text = stdio(&run(never.path()));
    let healthy_text = stdio(&run(healthy.path()));

    assert_eq!(
        body_without_verdict(&never_text),
        body_without_verdict(&healthy_text),
        "去掉 [run-once] 那行之后两份报告必须逐字节相同；\
         不同说明退出码之外还有别的东西跟着 run-state 变了：\n{never_text}\n---\n{healthy_text}"
    );
}

/// The promise this behaviour is held to is written down for users, not only
/// in the source. If someone decides "never ran" should be 0, this fails too,
/// so the doc cannot drift out of sync with the code silently.
#[test]
fn the_install_doc_still_promises_a_non_zero_exit_when_unhealthy() {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("install.md");
    let text = fs::read_to_string(&doc).expect("read docs/install.md");

    assert!(
        text.contains("it exits with a non-zero code"),
        "docs/install.md promises a non-zero exit when status is unhealthy; change this sentence before changing the behaviour"
    );
}
