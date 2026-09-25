//! W156 — R3: session lists in `chat-stasher ui` show a recognisable label per
//! session, per `.private/docs/29-UI-DESIGN.md` §2.2/§3.2/§3.3/§5.3.
//!
//! Everything here runs the real binary against a real rustic repository built
//! from a synthetic stage, over a real loopback socket. No machine's archive is
//! touched and no fixture line is real conversation text.
//!
//! Properties pinned (the design's three label states, plus the one corner it
//! names at machine level):
//!
//! * `activity-index` records a title per session row: the harness's own title
//!   (claude-code `ai-title`), else the first user line capped at 100
//!   characters and flagged when the cap cut anything — and two honest
//!   recorded states: `no label recorded` for content with nothing
//!   label-able, which per the design is also the word for a harness whose
//!   lines we do not read titles from yet (codex), and a row whose index
//!   predates titles, which the UI reports once per machine as
//!   `legacy_index`.
//! * `/sessions` shows every state in its own words; `/api/sessions` carries a
//!   tri-state `title` object per row and `machines_with_legacy_index` at the
//!   top level, at `schema_version: 2`.
//! * The pre-label machine is explained **once per machine** in a note at the
//!   top of the list, with the real destination-side repair command; its rows
//!   just say `label unknown`, and the note follows the machine filter.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

// --------------------------------------------------------------- sandbox

fn sandbox() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
}

/// Run the CLI with a sandboxed HOME/XDG so nothing reads or writes the real
/// machine's config, registry or stage.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W156 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    bin()
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

// --------------------------------------------------------------- fixtures

fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// One synthetic claude-code user line with a chosen content and timestamp.
fn cc_user(content: &str, ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"{content}"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// The harness's own session title, in the measured `ai-title` shape
/// (`aiTitle` is the field name; see the W156 report's shape measurement).
fn cc_ai_title(title: &str) -> String {
    format!(r#"{{"type":"ai-title","aiTitle":"{title}","sessionId":"s"}}"#)
}

/// A claude-code continuation summary, distinct from its `ai-title` record.
fn cc_summary(summary: &str) -> String {
    format!(r#"{{"type":"summary","summary":"{summary}","sessionId":"s"}}"#)
}

/// A metadata line that carries no conversation and no title.
fn cc_snapshot_meta() -> String {
    r#"{"type":"file-history-snapshot","sessionId":"s","uuid":"u7"}"#.to_string()
}

/// One synthetic codex message line (timestamped, but a harness whose lines we
/// do not read titles from — by design, `29-UI-DESIGN.md` §2.2: that is "no
/// label recorded", never a guess and never an empty string).
fn codex_user(content: &str, ts: &str) -> String {
    format!(
        r#"{{"timestamp":"{ts}","type":"user_message","payload":{{"message":{{"role":"user","content":"{content}"}}}},"cwd":"/x"}}"#
    )
}

/// A claude-code user line whose `content` is an array of typed blocks, with
/// the prompt text inside one `text` block (the shape real claude-code writes
/// for tool-bearing turns).
fn cc_user_blocks(text: &str, ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

const T: &str = "2025-01-15T12:00:00Z";

// Sessions of `mbp-w156`, one bucket, six rows:
//
// * TITLED     — user lines **and** an `ai-title` line placed *after* them, to
//                prove the harness title wins regardless of line order.
// * SUMMARISED — a user line only: the first user line serves as the label.
// * BLOCKS     — same, but `content` is the typed-blocks array shape.
// * SPARSE     — metadata only: an honest "no label recorded".
// * LONG       — a 150-character first user line: stored capped at 100
//                characters and flagged truncated.
// * CODEX      — a harness we do not read titles from: "no label recorded"
//                by design, not a fourth state.
const TITLED: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa01";
const SUMMARISED: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa02";
const SUMMARY_ONLY: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa07";
const BLOCKS: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa03";
const SPARSE: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa04";
const LONG: &str = "claude-code.mbp-w156.019bf00d-97b6-7eb2-9bf8-eacbacc0aa05";
const CODEX: &str = "codex.mbp-w156.99000001-2222-3333-4444-555555555555";

const TITLED_LABEL: &str = "Fix the parser retry loop";
const SUMMARISED_LABEL: &str = "sort a list of lists by length in python";
const SUMMARY_LABEL: &str = "continue the parser investigation";
const BLOCKS_LABEL: &str = "extract the retry policy from the config";
const CODEX_PROMPT: &str = "explain the ownership model of this repository";

/// The stage of `mbp-w156`, in one bucket, seven sessions.
fn stage_w156(sandbox: &Path) -> std::path::PathBuf {
    let stage = sandbox.join("stage-w156");
    let over_cap = {
        // Real-ish words, deliberately over the 100-character cap so the cut
        // has an honest body to take.
        let words = "triangulate a race in the idle timeout path of the loopback \
                     dashboard server by reading the socket close sequence and the \
                     request counters together, then reasoning about which side moved";
        assert!(
            words.chars().count() > 100,
            "fixture body must exceed the 100-char cap"
        );
        words.to_string()
    };
    let sessions: [(&str, Vec<String>); 7] = [
        (
            TITLED,
            vec![
                cc_user("please help me sort a failing retry case", T),
                cc_user("second line with more context", T),
                cc_ai_title(TITLED_LABEL),
            ],
        ),
        (SUMMARISED, vec![cc_user(SUMMARISED_LABEL, T)]),
        (SUMMARY_ONLY, vec![cc_summary(SUMMARY_LABEL)]),
        (BLOCKS, vec![cc_user_blocks(BLOCKS_LABEL, T)]),
        (SPARSE, vec![cc_snapshot_meta()]),
        (LONG, vec![cc_user(&over_cap, T)]),
        (CODEX, vec![codex_user(CODEX_PROMPT, T)]),
    ];
    for (session, lines) in &sessions {
        write_shard(&stage, "mbp-w156", session, lines);
    }
    stage
}

/// `mbp-legacy`: a machine whose activity index was written **before titles
/// existed** — the row has no `title` key at all. This is the state of every
/// real archive the moment this feature ships.
const LEGACY: &str = "claude-code.mbp-legacy.019bf00d-97b6-7eb2-9bf8-eaccaaa0bb01";
const LEGACY_PROMPT: &str = "the session that predates title extraction";

fn stage_legacy(sandbox: &Path) -> std::path::PathBuf {
    let stage = sandbox.join("stage-legacy");
    write_shard(&stage, "mbp-legacy", LEGACY, &[cc_user(LEGACY_PROMPT, T)]);
    // The hand-written, pre-title index: exactly the shape activity-index wrote
    // before the `title` field existed (title absent, source_zone accepted).
    let meta = stage.join("meta").join("mbp-legacy");
    fs::create_dir_all(&meta).unwrap();
    fs::write(
        meta.join("activity-v1.jsonl"),
        format!(
            r#"{{"session_id":"{LEGACY}","machine":"mbp-legacy","harness":"claude-code","first_unix":1736942400,"last_unix":1736942400,"line_count":1,"time_source":{{"kind":"exact"}},"source_zone":null}}"#
        ) + "\n",
    )
    .unwrap();
    stage
}

/// Build the repository: `mbp-w156` indexed by the real `activity-index`
/// (fresh title-bearing rows), `mbp-legacy` with its pre-title index pushed
/// as-is. Returns (repo, key).
fn build_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let stage = stage_w156(sandbox);
    let legacy = stage_legacy(sandbox);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-w156",
        ],
    );
    assert!(out.status.success(), "activity-index: {out:?}");
    // mbp-legacy deliberately does NOT get a fresh index: its hand-written one
    // is pushed as it is.
    for (machine, stage) in [("mbp-w156", &stage), ("mbp-legacy", &legacy)] {
        let push = run(
            sandbox,
            &[
                "push",
                "--stage",
                stage.to_str().unwrap(),
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key.to_str().unwrap(),
                "--machine",
                machine,
                "--keep-ssh-masters",
            ],
        );
        assert!(
            push.status.success(),
            "push {machine} failed: {:?}\n{}",
            push.status,
            String::from_utf8_lossy(&push.stderr)
        );
    }
    (repo, key)
}

// ----------------------------------------------------------------- socket

/// One `GET`, with the `Host` header the server's allowlist expects.
fn http(port: u16, target: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect loopback");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a complete response head");
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?}"));
    (status, body.to_string())
}

/// A running `ui` server: its port, its token, and the child to reap.
struct Ui {
    child: Child,
    port: u16,
    token: String,
}

impl Ui {
    /// Start `chat-stasher ui` and read its stdout until the URL appears.
    fn start(sandbox: &Path, extra: &[&str]) -> Ui {
        let home = sandbox.join("home");
        fs::create_dir_all(&home).unwrap();
        let stderr_path = sandbox.join("ui.stderr");
        let stderr = fs::File::create(&stderr_path).unwrap();
        let mut args: Vec<String> = vec![
            "ui".into(),
            "--no-open".into(),
            "--idle-timeout".into(),
            "180".into(),
            "--keep-ssh-masters".into(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut child = bin()
            .args(&args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", sandbox.join("config"))
            .env("XDG_DATA_HOME", sandbox.join("data"))
            .env("XDG_STATE_HOME", sandbox.join("state"))
            .env("CHAT_STASHER_REGISTRY", sandbox.join("registry.json"))
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn ui: {e}"));

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut url = None;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Some(rest) = line.trim().strip_prefix("http://") {
                url = Some(rest.to_string());
                break;
            }
            line.clear();
        }
        let url = url.unwrap_or_else(|| {
            panic!(
                "ui never printed a URL; stderr:\n{}",
                fs::read_to_string(&stderr_path).unwrap_or_default()
            )
        });
        let (addr, query) = url.split_once('/').expect("a path in the URL");
        let port: u16 = addr
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
            .unwrap_or_else(|| panic!("no port in {url}"));
        let token = query
            .split('&')
            .find_map(|kv| {
                kv.strip_prefix("?token=")
                    .or_else(|| kv.strip_prefix("token="))
            })
            .unwrap_or_else(|| panic!("no token in {url}"))
            .to_string();
        Ui { child, port, token }
    }

    fn get(&self, target: &str) -> (u16, String) {
        let sep = if target.contains('?') { '&' } else { '?' };
        http(self.port, &format!("{target}{sep}token={}", self.token))
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        // Best-effort teardown: the test's verdict was already decided by its
        // assertions, so a kill racing a clean exit is not a failure.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Teardown is best-effort by design; the verdict was decided above."
        )]
        let _ = self.child.kill();
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Teardown is best-effort by design; reaping may fail after exit."
        )]
        let _ = self.child.wait();
    }
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

// ------------------------------------------------------- activity-index rows

/// `activity-index` records one title state per session — the design's honest
/// words: the harness title (even when the `ai-title` line comes *after* the
/// user lines), the first user line (both content shapes, capped and flagged),
/// and "no label recorded" both for metadata-only content and for a harness we
/// do not read titles from (codex).
#[test]
fn activity_index_records_each_title_state() {
    let sb = sandbox();
    let stage = stage_w156(sb.path());
    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-w156",
        ],
    );
    assert!(out.status.success(), "activity-index: {out:?}");
    let index = fs::read_to_string(stage.join("meta/mbp-w156/activity-v1.jsonl")).unwrap();
    let rows: std::collections::BTreeMap<String, serde_json::Value> = index
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("index row must be JSON");
            (v["session_id"].as_str().unwrap().to_string(), v)
        })
        .collect();
    let row = |id: &str| {
        rows.get(id)
            .unwrap_or_else(|| panic!("{id} must be in the index: {index}"))
    };

    assert_eq!(row(TITLED)["title"]["state"], "known");
    assert_eq!(row(TITLED)["title"]["text"], TITLED_LABEL);
    assert_eq!(row(TITLED)["title"]["source"], "harness_title");
    assert_eq!(row(TITLED)["title"]["truncated"], false);
    assert_eq!(row(SUMMARISED)["title"]["source"], "first_user_line");
    assert_eq!(row(SUMMARISED)["title"]["text"], SUMMARISED_LABEL);
    assert_eq!(row(SUMMARY_ONLY)["title"]["state"], "known");
    assert_eq!(row(SUMMARY_ONLY)["title"]["source"], "harness_title");
    assert_eq!(row(SUMMARY_ONLY)["title"]["text"], SUMMARY_LABEL);
    assert_eq!(
        row(BLOCKS)["title"]["text"],
        BLOCKS_LABEL,
        "the typed-blocks content shape must be read too"
    );
    assert_eq!(row(BLOCKS)["title"]["source"], "first_user_line");
    assert_eq!(row(SPARSE)["title"]["state"], "no_label_recorded");
    // A harness whose lines we do not read titles from is "no label recorded"
    // by design (29-UI-DESIGN.md §2.2) — never a fourth state, never empty.
    assert_eq!(row(CODEX)["title"]["state"], "no_label_recorded");
    // The 150-char first line: capped at 100 characters, and flagged.
    assert_eq!(row(LONG)["title"]["state"], "known");
    assert_eq!(row(LONG)["title"]["source"], "first_user_line");
    assert_eq!(
        row(LONG)["title"]["text"]
            .as_str()
            .expect("text is a string")
            .chars()
            .count(),
        100
    );
    assert_eq!(row(LONG)["title"]["truncated"], true);
}

/// An over-cap first user line keeps its honest head: the stored text is the
/// first 100 characters, the cut is visible in the length alone, and nothing
/// of the tail survives into the index.
#[test]
fn a_truncated_label_keeps_only_the_capped_head() {
    let sb = sandbox();
    let stage = stage_w156(sb.path());
    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-w156",
        ],
    );
    assert!(out.status.success(), "activity-index: {out:?}");
    let index = fs::read_to_string(stage.join("meta/mbp-w156/activity-v1.jsonl")).unwrap();
    let long: serde_json::Value = index
        .lines()
        .map(|l| serde_json::from_str(l).expect("row is JSON"))
        .find(|v: &serde_json::Value| v["session_id"] == LONG)
        .expect("the LONG row");
    let text = long["title"]["text"].as_str().unwrap();
    assert!(text.starts_with("triangulate a race"));
    assert!(
        !text.contains("which side moved"),
        "the capped head must not still carry the tail: {text:?}"
    );
}

// ------------------------------------------------------------------- lists

/// The list page shows every title state in its own words: the two label texts
/// with their provenance on the row (in the cell's title attribute), "no label
/// recorded" for both the metadata-only session and the codex session, and
/// "label unknown" for the machine whose index predates titles.
#[test]
fn session_lists_show_each_title_state_in_its_own_words() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(
        sb.path(),
        &[
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );

    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    for text in [TITLED_LABEL, SUMMARISED_LABEL, BLOCKS_LABEL] {
        assert!(html.contains(text), "label text {text:?} missing: {html}");
    }
    assert!(
        count(&html, "no label recorded") >= 2,
        "SPARSE and CODEX must both be named as recordless, not empty: {html}"
    );
    assert!(
        count(&html, ">label unknown</td>") == 1,
        "the pre-title machine's one row must say label unknown, and nothing \
         else may: {html}"
    );
    assert!(
        html.contains("label source: the harness's own title"),
        "the harness title's provenance must travel with the row: {html}"
    );
    assert!(
        count(&html, "label source: the first user line") >= 3,
        "SUMMARISED, BLOCKS and LONG must each carry their provenance: {html}"
    );
    assert!(
        html.contains("…") || html.contains("&#8230;"),
        "a truncated label must be visibly marked: {html}"
    );
}

/// A machine whose index predates titles is explained **once per machine** in
/// a note at the top of the list — its rows just say "label unknown" — and
/// the note names the real destination-side repair command.
#[test]
fn a_pre_title_index_is_explained_once_per_machine() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(
        sb.path(),
        &[
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );

    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    assert!(
        count(&html, "Label coverage is partial") == 1,
        "exactly one coverage note, not one per row: {html}"
    );
    assert!(
        html.contains("mbp-legacy"),
        "the note must name the machine: {html}"
    );
    assert!(
        html.contains("predates"),
        "the note must give the reason in the design's words: {html}"
    );
    assert!(
        html.contains("activity-index --rebuild"),
        "the note must name the repair command it honestly runs: {html}"
    );
    // A machine whose index is fresh gets no coverage note about it.
    let (_, fresh) = ui.get("/sessions?machine=mbp-w156");
    assert!(
        !fresh.contains("Label coverage is partial"),
        "a fresh index must not read as partial: {fresh}"
    );
    let (_, only_legacy) = ui.get("/sessions?machine=mbp-legacy");
    assert!(
        only_legacy.contains("Label coverage is partial"),
        "the note is per-machine, so it must follow the machine: {only_legacy}"
    );
    assert_eq!(
        count(&only_legacy, ">label unknown</td>"),
        1,
        "{only_legacy}"
    );
}

/// `/api/sessions` carries a title object per row and the machine-level
/// legacy list, at schema_version 2 — never a bare string that an empty label
/// could be confused with a recorded absence.
#[test]
fn api_sessions_carries_title_states_at_schema_two() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(
        sb.path(),
        &[
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );

    let (status, body) = ui.get("/api/sessions");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema_version"], serde_json::json!(2), "{body}");
    assert_eq!(
        v["machines_with_legacy_index"],
        serde_json::json!(["mbp-legacy"]),
        "{body}"
    );
    let rows = v["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 8, "{body}");
    for r in rows {
        assert!(
            r["title"]["state"].is_string(),
            "every row carries a title object, never a bare string: {r}"
        );
    }
    let find = |pred: &dyn Fn(&serde_json::Value) -> bool| {
        rows.iter()
            .find(|r| pred(r))
            .unwrap_or_else(|| panic!("the row must exist: {body}"))
    };
    let titled = find(&|r| r["title"]["text"] == TITLED_LABEL);
    assert_eq!(titled["title"]["state"], "known");
    assert_eq!(titled["title"]["source"], "harness_title");
    assert_eq!(titled["title"]["truncated"], false);
    let long = find(&|r| r["title"]["truncated"] == true);
    assert_eq!(long["title"]["source"], "first_user_line");
    assert_eq!(long["title"]["text"].as_str().unwrap().chars().count(), 100);
    // SPARSE is the only mbp-w156 claude-code row with no label recorded;
    // CODEX carries the same state as the only codex row. The API's wire word
    // is the design's §5.3 one (`no_label`); the sidecar's own enum name
    // (`no_label_recorded`, asserted in the activity-index test) is internal.
    let sparse = find(&|r| {
        r["machine"] == "mbp-w156"
            && r["harness"] == "claude-code"
            && r["title"]["state"] == "no_label"
    });
    let codex = find(&|r| {
        r["machine"] == "mbp-w156" && r["harness"] == "codex" && r["title"]["state"] == "no_label"
    });
    for no_label in [sparse, codex] {
        assert_eq!(no_label["title"]["state"], "no_label", "{no_label}");
        assert!(
            no_label["title"]["text"].is_null(),
            "no_label carries no text to confuse with an empty one"
        );
    }
    let legacy = find(&|r| r["machine"] == "mbp-legacy");
    assert_eq!(legacy["title"]["state"], "legacy_index");
}

/// The session page shows the same label with the same honesty, next to the
/// metadata it belongs with.
#[test]
fn the_session_page_shows_the_label_row() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(
        sb.path(),
        &[
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );

    let (_, body) = ui.get("/api/sessions?machine=mbp-w156");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let titled = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["title"]["text"] == TITLED_LABEL)
        .expect("the titled session");
    let index = titled["index"].as_u64().unwrap();
    let (status, page) = ui.get(&format!("/session?i={index}"));
    assert_eq!(status, 200, "{page}");
    assert!(
        page.contains("<th>label</th>"),
        "the metadata table must have a label row: {page}"
    );
    assert!(page.contains(TITLED_LABEL), "{page}");
    assert!(
        page.contains("<th>label source</th>"),
        "the page must have a provenance row: {page}"
    );
    assert!(
        page.contains("the harness's own title"),
        "the page must say where the label came from: {page}"
    );

    // The pre-title machine's session: same word as the list, unknown — and no
    // provenance row, because nothing is known to attribute.
    let (_, body) = ui.get("/api/sessions?machine=mbp-legacy");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let index = v["sessions"][0]["index"].as_u64().unwrap();
    let (status, page) = ui.get(&format!("/session?i={index}"));
    assert_eq!(status, 200);
    assert!(page.contains("label unknown"), "{page}");
    assert!(!page.contains("no label recorded"), "{page}");
    assert!(
        !page.contains("<th>label source</th>"),
        "no provenance row for an unknown label: {page}"
    );
}
