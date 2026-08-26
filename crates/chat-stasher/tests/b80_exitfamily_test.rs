//! B80 — the third batch of "the report already admitted it, the exit code did not".
//!
//! The repository's exit-code contract (`main.rs`, established by B64/B73/B75
//! and the `cmd_search` / `cmd_collect` comments):
//!
//! ```text
//! 3 = 没读完 / 根本没读      1 = 读完了失败      2 = 用法错
//! ```
//!
//! Three commands printed a failure and then returned 0 anyway. Scripts and
//! timers only read the integer, so for them those runs were clean:
//!
//!   * `doctor` — `doctor: scan failed: …`, coverage marked unknown, exit 0.
//!   * `ingest` — `[ingest] errors : N`, exit 0.
//!   * `read --all-machines` — `WARN: … cannot read tree root`, exit 0.
//!     (Not on the ticket's list; found by grepping the family.)
//!
//! And one that was asked for and deliberately **not** given: `status` must
//! keep its exit code meaning "is the scheduled run healthy?", so an incomplete
//! scan (`HarnessProbe::unreadable_count`) still exits by the run-state verdict
//! alone. See the comment in `cmd_status`; `b78_statusgap_test.rs` already pins
//! that half. What `status` does get here is the sibling of `doctor`'s bug: a
//! scan that failed outright is 3, not the 1 it spends on a dead timer.
//!
//! **How the old behaviour was confirmed** (binary built at f0711af, before
//! this change), so a revert turns these red rather than merely not-green:
//!
//! ```text
//! $ HOME=$T CHAT_STASHER_REGISTRY=$T/bad.json chat-stasher doctor  ; echo $?   ->  0
//! $ HOME=$T CHAT_STASHER_REGISTRY=$T/bad.json chat-stasher status  ; echo $?   ->  1
//! $ chat-stasher ingest --inbox … --stage …   # one blocked bundle
//!   [ingest] errors           : 1                                 ; echo $?   ->  0
//! ```
//!
//! Everything below runs the real binary inside a `tempfile` sandbox with
//! `HOME` / `USERPROFILE` / `XDG_*` redirected into it. No real harness
//! directory, inbox, stage, archive or remote is touched, and no session body
//! is read by the test.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The exit codes this file is about, named so the assertions read as the
/// contract rather than as magic numbers.
const DID_NOT_FINISH: i32 = 3;
const FINISHED_AND_FAILED: i32 = 1;
const CLEAN: i32 = 0;

/// One `deepseek-<sessionId>.json` bundle, the documented `ingest` input.
const SESSION_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const BUNDLE: &str = r#"{"schema":"inbox@1","platform":"deepseek","sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","capturedAt":"2026-01-01T00:00:00Z","messages":[{"role":"user","content":"x"}]}"#;

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str], registry: Option<&Path>) -> std::process::Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create sandbox home");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    cmd.args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR");
    match registry {
        Some(path) => cmd.env("CHAT_STASHER_REGISTRY", path),
        None => cmd.env_remove("CHAT_STASHER_REGISTRY"),
    };
    cmd.output().expect("run chat-stasher")
}

/// A registry file that exists and will not parse. This is the one failure the
/// scanner refuses to paper over: `load_registry_from_repo` treats an explicit
/// `CHAT_STASHER_REGISTRY` as authoritative and returns the parse error
/// instead of falling back to the embedded copy.
fn unparseable_registry(sandbox: &Path) -> std::path::PathBuf {
    let path = sandbox.join("bad-registry.json");
    fs::write(&path, b"not json\n").expect("write unparseable registry");
    path
}

/// A registry with a single harness whose root does not exist in the sandbox:
/// the scan *succeeds* and finds nothing, which is the happy path for the
/// false-positive guards below.
fn empty_registry(sandbox: &Path) -> std::path::PathBuf {
    let cell = r#"{ "template": "~/no-such-harness-root", "format": "sqlite",
                    "confidence": "本机实测", "source": "B80 test fixture",
                    "sql_table": "t", "sql_id_column": "k",
                    "sql_required_columns": ["k"] }"#;
    let path = sandbox.join("empty-registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B80",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cell}, "linux": {cell}, "windows": {cell} }} }}
                  ] }}"#
        ),
    )
    .expect("write empty registry");
    path
}

fn stdio(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ---------------------------------------------------------------------------
// E1 — `doctor` said the coverage was unknown and exited 0
// ---------------------------------------------------------------------------

/// Pre-B80 this printed `doctor: scan failed: …` plus
/// `🔴 registry 缺失/无法解析 —— 会话覆盖未知` and returned **0**: a nightly
/// `chat-stasher doctor` that could not look at a single harness reported the
/// machine as fine. Reverting `cmd_doctor` to an unconditional `SUCCESS` fails
/// this assertion.
#[test]
fn doctor_that_could_not_scan_does_not_exit_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let registry = unparseable_registry(sandbox.path());
    let out = run(sandbox.path(), &["doctor"], Some(&registry));
    let text = stdio(&out);

    assert!(
        text.contains("scan failed"),
        "fixture must really hit the failed-scan branch; actual output:\n{text}"
    );
    assert!(
        text.contains("coverage unknown"),
        "the report must admit coverage is unknown first; actual output:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(DID_NOT_FINISH),
        "a scan that never started = nothing read = 3, not 1 (finished and failed), and certainly not 0; actual output:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// E2 — `ingest` printed `errors : N` and exited 0
// ---------------------------------------------------------------------------

/// Plant one bundle that cannot be sealed: a plain **file** sits where the
/// session's shard directory has to be, so `existing_file_shas` fails on it.
/// The candidate is opened and then fails — the "读完了失败" case — and the
/// file is left in the inbox unretired.
fn inbox_with_one_blocked_bundle(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let inbox = sandbox.join("inbox");
    let stage = sandbox.join("stage");
    fs::create_dir_all(&inbox).expect("create inbox");
    fs::create_dir_all(stage.join("sessions").join("b80-machine")).expect("create stage");
    fs::write(
        stage
            .join("sessions")
            .join("b80-machine")
            .join(format!("deepseek.{SESSION_ID}")),
        b"blocker\n",
    )
    .expect("plant blocker");
    fs::write(inbox.join(format!("deepseek-{SESSION_ID}.json")), BUNDLE).expect("plant bundle");
    (inbox, stage)
}

/// Pre-B80 this printed `[ingest] errors           : 1` and returned **0**, so
/// a scheduled `ingest` could leave plaintext bundles sitting in the download
/// directory forever while every run reported success. Reverting `cmd_ingest`
/// to an unconditional `SUCCESS` fails this assertion.
#[test]
fn ingest_that_could_not_consume_a_bundle_does_not_exit_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let (inbox, stage) = inbox_with_one_blocked_bundle(sandbox.path());
    let out = run(
        sandbox.path(),
        &[
            "ingest",
            "--inbox",
            inbox.to_str().expect("utf8 inbox"),
            "--stage",
            stage.to_str().expect("utf8 stage"),
            "--machine",
            "b80-machine",
        ],
        None,
    );
    let text = stdio(&out);

    assert!(
        text.contains("[ingest] errors"),
        "fixture must really produce an ingest error; 实际输出：\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(FINISHED_AND_FAILED),
        "收件箱被完整枚举了、这个文件被打开了然后失败 = 1，不是 3（根本没读）；实际输出：\n{text}"
    );
    assert!(
        inbox.join(format!("deepseek-{SESSION_ID}.json")).exists(),
        "失败的文件必须还留在 inbox 里 —— 退出码说的就是这件事；实际输出：\n{text}"
    );
}

// ---------------------------------------------------------------------------
// E3 — `read --all-machines` printed `WARN:` and exited 0 (not on the list)
// ---------------------------------------------------------------------------

/// The decision itself, at the level it is decidable without corrupting a
/// rustic pack: a `ReadAllReport` carrying a warning is not a complete read, so
/// `cmd_read_all_machines` owes it a 3. A snapshot whose tree root will not
/// open still contributes a `MachineMerge` with an empty session list, which is
/// byte-for-byte what "that machine backed nothing up" looks like — the whole
/// reason the code cannot stay 0.
#[test]
fn a_read_all_report_with_warnings_is_not_a_complete_read() {
    let mut report = chat_stasher::readback::ReadAllReport::default();
    assert!(
        report.complete(),
        "没有 warning 就是读全了 —— 正常路径必须留在 0"
    );
    report
        .warnings
        .push("host `h` snapshot deadbeef: cannot read tree root: fixture".to_string());
    assert!(
        !report.complete(),
        "有 warning = 有一台机器的会话没读出来 = 没读完，退出码必须是 {DID_NOT_FINISH}"
    );
}

// ---------------------------------------------------------------------------
// E4 — `status`: the scan-failure sibling, and the change deliberately NOT made
// ---------------------------------------------------------------------------

/// `status`'s scan-failure path used to spend **1**, the same code it spends on
/// "the timer is not running", so a caller could not tell an unreadable
/// registry from a dead scheduler. Nothing was scanned, so it belongs with
/// `doctor`'s answer: 3.
#[test]
fn status_that_could_not_scan_says_did_not_read_not_unhealthy_timer() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let registry = unparseable_registry(sandbox.path());
    let out = run(sandbox.path(), &["status"], Some(&registry));
    let text = stdio(&out);

    assert!(
        text.contains("status: scan failed"),
        "fixture must really hit the failed-scan branch; actual output:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(DID_NOT_FINISH),
        "根本没扫到 = 3；1 是这条命令留给「定时器不健康」的；实际输出：\n{text}"
    );
}

/// **The change this ticket asked for and did not make.** `status`'s exit code
/// carries exactly one meaning — is the scheduled run healthy — and an
/// incomplete scan is not allowed to overwrite it. `b78_statusgap_test.rs`
/// pins the positive half (clean and unreadable fixtures exit the same); this
/// pins the negative half from the other side: on a machine that never ran
/// `run-once`, `status` is non-zero for the *timer* reason, and it is 1 — the
/// completed-read-with-a-bad-verdict code — not 3.
#[test]
fn status_reserves_its_exit_code_for_the_timer_verdict() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let registry = empty_registry(sandbox.path());
    let out = run(sandbox.path(), &["status"], Some(&registry));
    let text = stdio(&out);

    assert!(
        !text.contains("scan failed"),
        "fixture 必须是「扫描成功了」的那条路径；实际输出：\n{text}"
    );
    assert!(
        text.contains("[run-once]"),
        "退出码的依据那一行必须还在；实际输出：\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(FINISHED_AND_FAILED),
        "扫描读完了、判定失败 = 1；把「扫描完整性」也塞进这个码会把这层含义冲掉；实际输出：\n{text}"
    );
}

// ---------------------------------------------------------------------------
// False-positive guards: every one of these commands still exits 0 when it
// actually succeeds. Without these, "return 3 unconditionally" passes the file.
// ---------------------------------------------------------------------------

/// `doctor` on a machine it can fully scan is still a clean run, **including**
/// when the report itself carries 🔴 risk lines: a risk is a successful
/// diagnosis with a bad finding, not a failure of `doctor`. This is also what
/// `scripts/release-gate.sh` step 7 asserts.
#[test]
fn a_scannable_machine_still_gets_doctor_exit_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let registry = empty_registry(sandbox.path());
    let out = run(sandbox.path(), &["doctor"], Some(&registry));
    let text = stdio(&out);

    assert!(
        !text.contains("scan failed"),
        "fixture 必须走扫描成功的那条路；实际输出：\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(CLEAN),
        "扫得动的机器上 doctor 必须还是 0；实际输出：\n{text}"
    );
}

/// `ingest` with nothing to complain about is still 0 — both the empty inbox
/// and the one-good-bundle case, so the guard covers the branch that writes a
/// shard as well as the branch that writes nothing.
#[test]
fn a_clean_ingest_still_exits_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let inbox = sandbox.path().join("inbox");
    let stage = sandbox.path().join("stage");
    fs::create_dir_all(&inbox).expect("create inbox");
    fs::create_dir_all(&stage).expect("create stage");
    let args = [
        "ingest",
        "--inbox",
        inbox.to_str().expect("utf8 inbox"),
        "--stage",
        stage.to_str().expect("utf8 stage"),
        "--machine",
        "b80-machine",
    ];

    let empty = run(sandbox.path(), &args, None);
    assert_eq!(
        empty.status.code(),
        Some(CLEAN),
        "空收件箱不是失败；实际输出：\n{}",
        stdio(&empty)
    );

    fs::write(inbox.join(format!("deepseek-{SESSION_ID}.json")), BUNDLE).expect("plant bundle");
    let good = run(sandbox.path(), &args, None);
    let text = stdio(&good);
    assert!(
        text.contains("[ingest] consumed         : 1"),
        "fixture 必须真的封出一个分片；实际输出：\n{text}"
    );
    assert!(
        !text.contains("[ingest] errors"),
        "正常路径不许出现 errors 行；实际输出：\n{text}"
    );
    assert_eq!(
        good.status.code(),
        Some(CLEAN),
        "一次干净的 ingest 必须还是 0；实际输出：\n{text}"
    );
}
