//! W15 — `chat-stasher ui`, the overview dashboard, end to end.
//!
//! Everything here runs against a real rustic repository built from a synthetic
//! stage, through the real binary and a real loopback socket. Nothing is
//! mocked, and no machine's archive is touched.
//!
//! The properties pinned, in the order they matter:
//!
//! * A drill-down and `chat-stasher search` with the same flags return the
//!   **same set** — because both apply the shared selector
//!   ([`chat_stasher::selector`], ADR-027) to the same rows.
//! * The dashboard never fetches conversation payload unless the content route
//!   is asked for explicitly, and the cost is on the page before it is.
//! * `view` still works, and says on stderr that it is now an alias.
//! * Extension-delivered sessions (`deepseek.…`) are in the activity index and
//!   therefore on the dashboard, with the reason their time is unknown.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

// --------------------------------------------------------------- fixture

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
        r#"{"schema_version":1,"generated":"W15 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    bin()
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// One synthetic extension-delivered line: no timestamp field at all, which is
/// exactly what `chat-stasher inbox` archives for a web-chat platform.
fn ext_line() -> String {
    r#"{"id":"sess-0001","title":"a conversation","capturedAt":"2025-02-01T00:00:00.000Z","messages":[]}"#.to_string()
}

fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// The two sessions of `mbp-a`:
///
/// * `WIDE` is one conversation spanning two whole days. It is chosen so that
///   the local-day window 2025-01-15 .. 2025-01-16 **intersects** it in every
///   timezone on earth, which is what makes the drill-down equality below a
///   meaningful assertion rather than one that passes because both sides are
///   empty.
/// * `OLD` is a different conversation, far outside that window.
const WIDE: &str = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
const OLD: &str = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09766";
const EXT: &str = "deepseek.sess-0001";
const NO_INDEX: &str = "claude-code.mbp-c.019bf00d-97b6-7eb2-9bf8-eacbacc09767";

/// One stage per machine: `push` refuses a stage that holds more than one
/// machine's partition (`stage machine mismatch`), because a stage *is* one
/// machine's outbox. Three machines therefore means three stages and three
/// snapshots, which is also what a real archive looks like.
fn stage_for(
    sandbox: &Path,
    machine: &str,
    sessions: &[(&str, Vec<String>)],
) -> std::path::PathBuf {
    let stage = sandbox.join(format!("stage-{machine}"));
    for (session, lines) in sessions {
        write_shard(&stage, machine, session, lines);
    }
    stage
}

/// The stage of one machine, ready to push.
fn stage_a(sandbox: &Path) -> std::path::PathBuf {
    stage_for(
        sandbox,
        "mbp-a",
        &[
            (
                WIDE,
                vec![
                    cc_line("2025-01-15T00:00:00Z"),
                    cc_line("2025-01-16T23:59:59Z"),
                ],
            ),
            (OLD, vec![cc_line("2020-06-01T00:00:00Z")]),
        ],
    )
}

/// `mbp-c` never ran `activity-index`, so its snapshot has no sidecar. That is
/// the state the dashboard must name rather than render as "no sessions".
fn stage_c(sandbox: &Path) -> std::path::PathBuf {
    stage_for(
        sandbox,
        "mbp-c",
        &[(NO_INDEX, vec![cc_line("2025-01-15T12:00:00Z")])],
    )
}

/// Build the repository: three machines, `mbp-a` and `mbp-b` indexed (the
/// latter holds the extension-delivered session), `mbp-c` deliberately not.
fn build_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let stages = [
        stage_a(sandbox),
        stage_for(sandbox, "mbp-b", &[(EXT, vec![ext_line()])]),
        stage_c(sandbox),
    ];
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    for (machine, stage) in ["mbp-a", "mbp-b", "mbp-c"].iter().zip(stages.iter()) {
        if *machine != "mbp-c" {
            let out = run(
                sandbox,
                &[
                    "activity-index",
                    "--stage",
                    stage.to_str().unwrap(),
                    "--machine",
                    machine,
                ],
            );
            assert!(out.status.success(), "activity-index {machine}: {out:?}");
        }
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

/// A repository whose only snapshot holds machine metadata and no sessions —
/// the ADR-021 shape of a declared machine, the stage shape `push` accepts
/// with meta files and no shards. This is the archive that used to stop `ui`
/// before it served anything (OQ-2's empty state, W172).
fn empty_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let stage = sandbox.join("stage-meta");
    let meta = stage.join("meta").join("mbp-empty");
    fs::create_dir_all(&meta).unwrap();
    // Schema-correct by construction: every `MachineDeclaration` field
    // (identity.rs) is present, so no reader can fail on it rather than read.
    fs::write(
        meta.join("machine.json"),
        concat!(
            "{\n",
            "  \"machine_id\": \"mbp-empty\",\n",
            "  \"display_name\": \"mbp-empty\",\n",
            "  \"os\": \"synthetic\",\n",
            "  \"first_seen_unix\": 0,\n",
            "  \"declared_harnesses\": []\n",
            "}\n"
        ),
    )
    .unwrap();
    let repo = sandbox.join("repo-empty");
    let key = sandbox.join("keys-empty").join("masterkey.json");
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
            "mbp-empty",
            "--keep-ssh-masters",
        ],
    );
    assert!(
        push.status.success(),
        "meta-only push failed: {:?}\n{}",
        push.status,
        String::from_utf8_lossy(&push.stderr)
    );
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
            // A reset on a loopback socket means the peer closed while our
            // receive buffer still held data. Everything it wrote is already
            // here; the `Content-Length` check below is what proves that.
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
    let declared: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or_else(|| panic!("no Content-Length in {head:?}"));
    assert_eq!(
        body.len(),
        declared,
        "the response body is shorter than its own Content-Length, so a truncation \
         would otherwise read as a correct answer"
    );
    (status, body.to_string())
}

/// The first JSON value on a stream.
///
/// `search --json` prints one object and then `reap_remote` appends a
/// `[reap] …` line to **stdout** (a defect in the `--json` contract, noted in
/// the W15 report and left alone here because it is not this change's
/// surface). Reading the first value keeps this test measuring the report
/// rather than the trailing chatter.
fn first_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next()
        .expect("search --json must print an object")
        .expect("the first value must parse")
}

#[test]
fn text_scan_reports_mode_and_keeps_metadata_gaps_visible() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let out = run(
        sb.path(),
        &[
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            "mbp-a",
            "--text",
            "hi",
            "--scan",
            "--json",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let result = first_json(&out.stdout);
    assert_eq!(result["mode"], "scan");
    assert_eq!(result["selected"], 2);
    assert_eq!(result["matched"], 2);
    assert_eq!(result["read_failures"], 0);
    assert_eq!(result["metadata_unreadable_parts"], 0);
    assert_eq!(result["unplaceable_sessions"], 0);
    assert_eq!(result["metadata_answer_complete"], true);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("hi"));

    let partial = run(
        sb.path(),
        &[
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            "mbp-c",
            "--day",
            "2025-01-15",
            "--text",
            "hi",
            "--scan",
            "--json",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(partial.status.code(), Some(3));
    let partial = first_json(&partial.stdout);
    assert_eq!(partial["metadata_unreadable_parts"], 0);
    assert_eq!(partial["unplaceable_sessions"], 1);
    assert_eq!(partial["metadata_answer_complete"], false);
}

#[test]
fn text_fts_marks_a_valid_but_incomplete_index_unknown() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let repo_arg = repo.to_str().unwrap();
    let key_arg = key.to_str().unwrap();
    let built = run(
        sb.path(),
        &[
            "index",
            "build",
            "--repo",
            repo_arg,
            "--key-file",
            key_arg,
            "--keep-ssh-masters",
        ],
    );
    assert!(built.status.success(), "index build failed: {built:?}");

    let stage = stage_a(sb.path());
    let added_id = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09768";
    let added_line = cc_line("2025-01-15T13:00:00Z").replace("hi", "needlepeach");
    write_shard(&stage, "mbp-a", added_id, &[added_line]);
    let indexed = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-a",
        ],
    );
    assert!(
        indexed.status.success(),
        "activity-index failed: {indexed:?}"
    );
    let pushed = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo_arg,
            "--key-file",
            key_arg,
            "--machine",
            "mbp-a",
            "--keep-ssh-masters",
        ],
    );
    assert!(pushed.status.success(), "push failed: {pushed:?}");

    let out = run(
        sb.path(),
        &[
            "search",
            "--repo",
            repo_arg,
            "--key-file",
            key_arg,
            "--machine",
            "mbp-a",
            "--text",
            "needlepeach",
            "--json",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
    let result = first_json(&out.stdout);
    assert_eq!(result["mode"], "fts");
    assert_eq!(result["selected"], 3);
    assert_eq!(result["matched"], 0);
    assert_eq!(result["index_covered"], 2);
    assert_eq!(result["index_missing"], 1);
}

/// A running `ui` server: its port, its token, and the child to reap.
struct Ui {
    child: Child,
    port: u16,
    token: String,
}

impl Ui {
    /// Start `chat-stasher ui` and read its stdout until the URL appears.
    ///
    /// The stderr is redirected to a file so a chatty failure cannot fill a pipe
    /// and deadlock `wait`.
    fn start(sandbox: &Path, repo: &Path, key: &Path, extra: &[&str], subcommand: &str) -> Ui {
        let home = sandbox.join("home");
        let registry = sandbox.join("registry.json");
        fs::create_dir_all(&home).unwrap();
        let stderr_path = sandbox.join(format!("{subcommand}.stderr"));
        let stderr = fs::File::create(&stderr_path).unwrap();
        let mut args: Vec<String> = vec![
            subcommand.to_string(),
            "--repo".into(),
            repo.to_str().unwrap().into(),
            "--key-file".into(),
            key.to_str().unwrap().into(),
            "--no-open".into(),
            "--idle-timeout".into(),
            "5".into(),
            "--keep-ssh-masters".into(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut child = bin()
            .args(&args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", sandbox.join("config"))
            .env("XDG_DATA_HOME", sandbox.join("data"))
            .env("XDG_STATE_HOME", sandbox.join("state"))
            .env("CHAT_STASHER_REGISTRY", &registry)
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("spawn ui");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut url = None;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("http://") {
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
        // Best-effort teardown: the server has a 5 s idle timeout and the
        // assertions above are already made, so a kill that races with a clean
        // exit is not a failure. Asserting here would replace the test's real
        // verdict with a teardown problem.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Teardown is best-effort by design; the test's verdict was already decided by its assertions."
        )]
        let _ = self.child.kill();
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Teardown is best-effort by design; reaping may fail if the process already exited."
        )]
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------------- tests

/// The `deepseek.…` question, answered by looking at the file rather than at a
/// claim: `activity-index` enumerates every session directory under
/// `sessions/<machine>/` and derives the harness from the id prefix, with no
/// allowlist of harnesses. So an extension-delivered session **is** in the
/// index — with a row whose `time_source` is `unknown`, because `activity`
/// knows no timestamp shape for those platforms.
#[test]
fn extension_delivered_sessions_are_in_the_activity_index() {
    let sb = sandbox();
    let stage = stage_for(sb.path(), "mbp-b", &[(EXT, vec![ext_line()])]);
    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-b",
        ],
    );
    assert!(out.status.success(), "activity-index: {out:?}");
    let index = fs::read_to_string(stage.join("meta/mbp-b/activity-v1.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(index.lines().next().unwrap()).unwrap();
    assert_eq!(row["session_id"], serde_json::json!(EXT));
    assert_eq!(
        row["harness"],
        serde_json::json!("deepseek"),
        "a web-chat platform is a source like any other: {index}"
    );
    assert_eq!(
        row["time_source"]["kind"],
        serde_json::json!("unknown"),
        "its time is unknown, and the index says so rather than guessing: {index}"
    );
}

/// The headline. The dashboard names every machine, counts every session and
/// byte, shows the extension-delivered source, and marks the machine that never
/// ran `activity-index` as missing one.
#[test]
fn the_overview_page_is_the_archive_at_a_glance() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/");
    assert_eq!(status, 200);
    for machine in ["mbp-a", "mbp-b", "mbp-c"] {
        assert!(
            html.contains(machine),
            "machine {machine} missing from {html}"
        );
    }
    assert!(html.contains("claude-code"), "{html}");
    assert!(
        html.contains("deepseek"),
        "the extension source must appear: {html}"
    );
    assert!(
        html.contains("MISSING"),
        "mbp-c has a snapshot and no index; it must be named, not shown empty: {html}"
    );
    assert!(
        !html.contains("index missing</span> · \n"),
        "sanity: the health word and its rendering must be intact"
    );
    assert!(
        html.contains("Metadata tier"),
        "the tier must be stated: {html}"
    );

    // 4 sessions across the tree: 2 on mbp-a, 1 on mbp-b, 1 on mbp-c.
    let (status, json) = ui.get("/api/overview");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["summary"]["sessions_in_view"], serde_json::json!(4));
    assert_eq!(
        v["summary"]["time_unknown"],
        serde_json::json!(2),
        "the deepseek session and mbp-c's session both have no known time: {json}"
    );
    assert_eq!(v["payload_loaded"], serde_json::json!(false));
    assert_eq!(v["data_blobs_read"], serde_json::json!(0));
}

/// W172 / R9: the machine that never ran `activity-index` (the `mbp-c` stage
/// above is the repro fixture) must reach the HTML exactly the way it already
/// reaches the JSON — a banner naming the machines
/// `machines_without_activity_index` lists, and a heatmap row that reads
/// unknown, not empty, with the reason and the repair command on the page.
#[test]
fn a_machine_without_an_index_surfaces_on_the_page_as_it_does_in_the_json() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/");
    assert_eq!(status, 200);
    assert!(
        html.contains("Machines without an activity index."),
        "the banner must render: {html}"
    );
    assert!(
        html.contains("1 machine(s) hold sessions but no activity index"),
        "{html}"
    );
    assert!(
        html.contains("<li class=mono>mbp-c</li>"),
        "the machine list must name each one: {html}"
    );
    assert!(
        html.contains("chat-stasher activity-index"),
        "the banner must name the repair command: {html}"
    );

    let (status, json) = ui.get("/api/overview");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["machines_without_activity_index"],
        serde_json::json!(["mbp-c"]),
        "the banner and the JSON field must name the same machines: {json}"
    );

    // The heatmap: mbp-c's row is unknown-marked, with the reason on the page
    // instead of a row of empty cells.
    let heat = html
        .split("<h2>Activity, by week</h2>")
        .nth(1)
        .expect("the heatmap section must exist");
    assert!(
        heat.contains("UNKNOWN — no activity index for this machine"),
        "the week cells must not read as empty: {heat}"
    );
    assert!(
        heat.contains(
            "<span class=mono>mbp-c</span> has no activity index, so its sessions cannot be \
             placed in time"
        ),
        "the reason line must name the machine in the CLI's own words: {heat}"
    );
    assert!(
        !heat.contains("machine=mbp-c&since="),
        "an unmeasured week must not link a time filter: {heat}"
    );
}

/// W172 / OQ-2: a metadata read that finished with nothing to show still
/// serves. This archive used to stop `ui` before any socket existed ("nothing
/// to show, so no server was started", exit 1); the honest empty page is what
/// you get now, and the exit is 0 once the server idles out.
#[test]
fn an_archive_that_holds_nothing_still_serves_its_honest_empty_page() {
    let sb = sandbox();
    let (repo, key) = empty_repo(sb.path());

    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let (status, html) = ui.get("/");
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("(this destination holds no sessions)"),
        "the empty page must state the measured absence: {html}"
    );
    let (status, json) = ui.get("/api/overview");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["summary"]["sessions_in_view"],
        serde_json::json!(0),
        "{json}"
    );
    assert_eq!(v["snapshots_scanned"], serde_json::json!(1), "{json}");
    assert_eq!(
        v["machines_without_activity_index"],
        serde_json::json!([]),
        "mbp-empty holds no sessions, so it is not the no-index state: {json}"
    );
    drop(ui);

    // And run one to completion: served, then exited 0 — with the URL printed
    // and the shared selector's own no-hit line, never the pre-bind stop.
    let out = run(
        sb.path(),
        &[
            "ui",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--no-open",
            "--idle-timeout",
            "1",
            "--keep-ssh-masters",
        ],
    );
    assert!(
        out.status.success(),
        "a served empty dashboard exits 0: {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("http://"),
        "the URL must still be printed: {stdout}"
    );
    assert!(
        !stdout.contains("no server was started"),
        "the pre-socket exit is gone: {stdout}"
    );
    assert!(
        stdout.contains("0 of 0 sessions matched"),
        "the shared selector's no-hit line keeps the honest distinction: {stdout}"
    );
    assert!(
        stdout.contains("in view / 0 in the archive"),
        "the narration must quote the archive, not the launch-filtered set: {stdout}"
    );
}

#[test]
fn no_content_section_renders_when_time_unknown_is_zero() {
    let sb = sandbox();
    let machine = "m-metadata";
    let session = "claude-code.m-metadata.aaaaaaaa-0000-0000-0000-000000000098";
    let summary = r#"{"type":"summary","sessionId":"s","uuid":"u9","summary":"synthetic","timestamp":"2025-01-15T12:00:00Z"}"#.to_string();
    let stage = stage_for(sb.path(), machine, &[(session, vec![summary])]);
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");

    let indexed = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        indexed.status.success(),
        "activity-index failed: {indexed:?}"
    );
    let pushed = run(
        sb.path(),
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
    assert!(pushed.status.success(), "push failed: {pushed:?}");

    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let (status, html) = ui.get("/");
    assert_eq!(status, 200);
    assert!(html.contains("<h2>No conversation content</h2>"), "{html}");
    assert!(
        !html.contains("every session in view has a recorded conversation time"),
        "{html}"
    );
    let (status, sessions_page) = ui.get("/sessions");
    assert_eq!(status, 200);
    assert!(
        sessions_page.contains("no conversation content"),
        "{sessions_page}"
    );
    assert!(
        !sessions_page.contains(">unknown</span>"),
        "{sessions_page}"
    );
    let (status, session_page) = ui.get("/session?i=0");
    assert_eq!(status, 200);
    assert!(
        session_page.contains("no conversation content"),
        "{session_page}"
    );
    assert!(!session_page.contains(">unknown</b>"), "{session_page}");
    let (status, json) = ui.get("/api/overview");
    assert_eq!(status, 200);
    let overview: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(overview["summary"]["time_unknown"], serde_json::json!(0));
    let (status, sessions) = ui.get("/api/sessions");
    assert_eq!(status, 200);
    let sessions: serde_json::Value = serde_json::from_str(&sessions).unwrap();
    assert_eq!(
        sessions["sessions"][0]["first_unix"]["kind"],
        "no_conversation_content"
    );
    assert_eq!(
        sessions["sessions"][0]["last_unix"]["kind"],
        "no_conversation_content"
    );
}

/// **The pin.** A drill-down link and `chat-stasher search` with the same flags
/// return the same sessions — because the page applies the shared selector to
/// one unfiltered read, and `search` applies it to the same rows.
#[test]
fn a_drill_down_returns_exactly_what_search_returns() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // (query string, whether it must match something non-empty — a case where
    // both sides are empty would prove nothing about equality)
    let cases: [(&str, bool); 5] = [
        ("machine=mbp-a", true),
        ("harness=claude-code", true),
        ("machine=mbp-a&harness=claude-code", true),
        // The same conversation interval is compared on both sides; the window
        // happens to be satisfied by the fixture in every timezone (see `WIDE`).
        ("machine=mbp-a&since=2025-01-15&until=2025-01-16", true),
        // A filter that matches nothing: the two must still agree, and the
        // agreement must not be an artefact of both returning an empty list.
        ("machine=does-not-exist", false),
    ];

    for (query, must_match) in cases {
        let (status, body) = ui.get(&format!("/api/sessions?{query}"));
        assert_eq!(status, 200, "{query}: {body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        let mut from_ui: Vec<String> = page["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_short_id"].as_str().unwrap().to_string())
            .collect();

        // `search` takes the same filters. The query string is turned back into
        // argv verbatim, so the two paths are literally the same flags.
        let mut args: Vec<String> = vec!["search".into(), "--json".into()];
        args.push("--repo".into());
        args.push(repo.to_str().unwrap().into());
        args.push("--key-file".into());
        args.push(key.to_str().unwrap().into());
        args.push("--keep-ssh-masters".into());
        for q in query.split('&') {
            let (k, v) = q.split_once('=').unwrap();
            args.push(format!("--{k}"));
            args.push(v.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(sb.path(), &arg_refs);
        // `search` exits 1 when it read the whole destination and matched
        // nothing — an answer, not a failure. `must_match` decides which is
        // expected, so a filter that silently stopped matching cannot pass.
        let code = out.status.code();
        assert_eq!(
            code,
            Some(if must_match { 0 } else { 1 }),
            "search exited {code:?} for `{query}`, which is neither the \"matched\" nor the \
             \"read it all, nothing there\" code"
        );
        let searched = first_json(&out.stdout);
        let mut from_search: Vec<String> = searched["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_short_id"].as_str().unwrap().to_string())
            .collect();

        from_ui.sort();
        from_search.sort();
        assert_eq!(
            from_ui, from_search,
            "`{query}`: the dashboard listed {from_ui:?} but search listed {from_search:?}"
        );
        assert!(
            !must_match || !from_ui.is_empty(),
            "`{query}`: expected a non-empty match on both sides"
        );
        if must_match {
            assert!(
                !from_ui.is_empty(),
                "`{query}` matched nothing, so the equality above proved nothing"
            );
        }
        // Parsing must have seen the same counts too, not just the same ids.
        assert_eq!(
            page["not_matched"].as_u64().unwrap_or(0)
                + page["could_not_be_placed"].as_u64().unwrap_or(0),
            searched["not_matched"].as_u64().unwrap_or(0)
                + searched["could_not_be_placed"].as_u64().unwrap_or(0),
            "`{query}`: the pages disagree about the sessions that did NOT match"
        );
    }
}

/// The window case specifically: the fixture's wide conversation spans two
/// whole days, so a two-day local window intersects it in every timezone. One
/// of the two mbp-a sessions must therefore come back, and the other must not.
#[test]
fn a_time_window_drill_down_is_not_vacuous() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let (status, body) = ui.get("/api/sessions?machine=mbp-a&since=2025-01-15&until=2025-01-16");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ids: Vec<&str> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_short_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "expected exactly the wide session, got {ids:?}"
    );
    assert!(
        v["not_matched"].as_u64().unwrap() >= 1,
        "the 2020 session must have been evaluated and rejected: {body}"
    );
}

/// The cost is printed before the payload is fetched, and the payload is only
/// fetched by the route the click leads to.
#[test]
fn content_is_fetched_only_when_the_session_page_is_clicked_through() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // Find the wide session's row index, then look at its page.
    let (_, body) = ui.get("/api/sessions?machine=mbp-a");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let row = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_short_id"].as_str().unwrap() == "claude-code~0e6f0b")
        .or_else(|| v["sessions"].as_array().unwrap().first())
        .expect("at least one session");
    let index = row["index"].as_u64().unwrap();

    let (status, page) = ui.get(&format!("/session?i={index}"));
    assert_eq!(status, 200);
    assert!(page.contains("Body not loaded"), "{page}");
    assert!(
        page.contains("Loading this session costs"),
        "the price must be on the page: {page}"
    );
    assert!(
        page.contains(&format!("/content?i={index}")),
        "the load must be an explicit link: {page}"
    );
    assert!(
        !page.contains("Concatened"),
        "the page must not have rendered content"
    );
    assert!(
        !page.contains("\"parentUuid\""),
        "no conversation payload may be on the session page: {page}"
    );

    // The click.
    let (status, content) = ui.get(&format!("/content?i={index}"));
    assert_eq!(status, 200);
    assert!(
        content.contains("parentUuid"),
        "the content route must return the archived JSONL: {content}"
    );
    assert!(
        content.contains("sha256="),
        "the same digest `read` prints must be on the page: {content}"
    );
}

/// Over a real socket: no token is a 403, and a non-GET is a 405 even with one.
#[test]
fn the_socket_refuses_without_a_token_and_refuses_anything_but_get() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, body) = http(ui.port, "/");
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("token"), "{body}");
    let (status, _) = http(ui.port, "/?token=not-the-token");
    assert_eq!(status, 403);

    let mut stream = TcpStream::connect(("127.0.0.1", ui.port)).unwrap();
    write!(
        stream,
        "POST /?token={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        ui.token, ui.port
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 405"),
        "only GET is accepted: {text}"
    );

    // The instrument can say yes on this exact socket.
    let (status, _) = ui.get("/");
    assert_eq!(status, 200);
}

/// `view` still works, and says what it now is. One line, on stderr.
#[test]
fn the_view_alias_is_deprecated_on_stderr_and_behaves_like_ui() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "view");
    let (status, html) = ui.get("/");
    assert_eq!(status, 200, "the alias must serve the dashboard");
    assert!(html.contains("chat-stasher"), "{html}");
    drop(ui);
    let stderr = fs::read_to_string(sb.path().join("view.stderr")).unwrap();
    assert!(
        stderr.contains("deprecated") && stderr.contains("alias for `chat-stasher ui`"),
        "the alias must say so on stderr: {stderr}"
    );
    assert_eq!(
        stderr.lines().filter(|l| l.contains("deprecated")).count(),
        1,
        "exactly one deprecation line: {stderr}"
    );
}

/// **A stdout that nobody reads must not take the dashboard down.**
///
/// This is the W23 flake, made deterministic. `Ui::start` above takes the
/// child's stdout, reads until the URL, and drops the reader; `println!` panics
/// when its write fails, so the *next* line `ui` printed raised `Broken pipe`
/// and the process died before `serve` was entered. The tests then failed at
/// `w15_ui_test.rs:210` with `raw="" bytes=0` — a zero-byte response from a
/// socket whose process was already gone. Every observed failure was on the
/// *first* request after a `Ui::start`, which is the only window this covers.
///
/// The harness's behaviour is legitimate (a supervisor that reads one line and
/// stops is a normal client, and `chat-stasher ui | head -1` is the same
/// shape), so the fix is in the binary: narration is not the product. Here the
/// pipe is closed at spawn rather than after the URL — the widest form of the
/// window, and the only one that is deterministic rather than a race. Unfixed,
/// the first narration line kills the launch and the exit code is 101.
#[test]
fn a_closed_stdout_does_not_fail_the_launch() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let home = sb.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut child = bin()
        .args([
            "ui",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--no-open",
            "--idle-timeout",
            "1",
            "--keep-ssh-masters",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sb.path().join("config"))
        .env("XDG_DATA_HOME", sb.path().join("data"))
        .env("XDG_STATE_HOME", sb.path().join("state"))
        .env("CHAT_STASHER_REGISTRY", sb.path().join("registry.json"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ui");
    // Every line the child writes from here on fails. Nothing about that is a
    // reason for the dashboard to die.
    drop(child.stdout.take());
    let status = child.wait().expect("wait for the ui child");
    assert_eq!(
        status.code(),
        Some(0),
        "a launch whose stdout nobody reads must still start, serve its idle \
         timeout out, and exit cleanly"
    );
}

/// The launch filter is the shared selector too, and the page says which filter
/// is in force rather than quietly showing a subset.
#[test]
fn a_launch_filter_is_named_on_the_page() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &["--machine", "mbp-a"], "ui");
    let (status, html) = ui.get("/");
    assert_eq!(status, 200);
    assert!(html.contains("Filter in force"), "{html}");
    assert!(html.contains("mbp-a"), "{html}");
    assert!(
        html.contains("sessions in view"),
        "the headline must say it is a filtered count: {html}"
    );
    // …and the filter is still applied by the shared selector, so a drill-down
    // inside it is the same query `search` would answer.
    let (_, body) = ui.get("/api/sessions");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["summary"],
        serde_json::Value::Null,
        "sanity: /api/sessions is the list shape, not the overview shape"
    );
    assert_eq!(v["matched"], serde_json::json!(2));
}

// ------------------------------------------------------- UIA-2 · paging (W175)
//
// The §5.2 contract's four invariants over a real loopback socket and a real
// binary: `limit=0` is a usage error; an offset past the end is a 200 with
// an empty window; a concatenated walk of the pages is the whole list in the
// walk's order; and no page of any sort can leave the launch filter's set.

/// The ten-row paging archive: one machine, nine claude-code conversations
/// stamped one per day (so every time order is derivable by hand — a later
/// day is a later conversation time, and one line per session makes
/// `first == last`), plus one extension-delivered deepseek session with no
/// timestamp at all, the row every time order must refuse to rank.
const PAGE_DAYS: u32 = 9;

fn page_stage_session(day: u32) -> String {
    format!(
        "claude-code.mbp-page.aaaaaaaa-0000-0000-0000-{:06}",
        day as u64 + 1
    )
}

fn build_page_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let mut sessions: Vec<(String, Vec<String>)> = (1..=PAGE_DAYS)
        .map(|day| {
            (
                page_stage_session(day),
                vec![cc_line(&format!("2025-03-{day:02}T12:00:00Z"))],
            )
        })
        .collect();
    sessions.push(("deepseek.page-0001".to_string(), vec![ext_line()]));
    let pairs = sessions
        .iter()
        .map(|(id, lines)| (id.as_str(), lines.clone()))
        .collect::<Vec<_>>();
    let stage = stage_for(sandbox, "mbp-page", &pairs);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let indexed = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-page",
        ],
    );
    assert!(indexed.status.success(), "activity-index: {indexed:?}");
    let pushed = run(
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
            "mbp-page",
            "--keep-ssh-masters",
        ],
    );
    assert!(
        pushed.status.success(),
        "push failed: {pushed:?}\n{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    (repo, key)
}

/// The `sessions` array of one JSON page, as `(machine, last_unix value or
/// why-kind)` pairs — machine and the conversation time are the two facts
/// the paging tests reason about, and neither is conversation text.
fn rows_of(body: &str) -> Vec<(String, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["machine"].as_str().unwrap().to_string(),
                r["last_unix"].clone(),
            )
        })
        .collect()
}

#[test]
fn a_zero_limit_and_unknown_sorts_are_usage_errors_on_the_socket() {
    let sb = sandbox();
    let (repo, key) = build_page_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // `limit=0` — the one value most easily read as "just show me nothing".
    // It is refused as a usage error on both routes, never answered with a
    // list of zero rows.
    for (target, needle) in [
        ("/sessions?limit=0", "`limit`"),
        ("/sessions?limit=x", "`limit`"),
        ("/api/sessions?limit=0", "`limit`"),
        ("/sessions?offset=x", "`offset`"),
        ("/api/sessions?offset=x", "`offset`"),
        ("/sessions?sort=by-time", "`sort`"),
        ("/sessions?sort=", "`sort`"),
    ] {
        let (status, body) = ui.get(target);
        assert_eq!(status, 400, "{target}: {body}");
        assert!(
            body.contains(needle),
            "{target} must explain itself: {body}"
        );
        assert!(
            !body.contains("Not in this destination"),
            "{target}: a refused query must not read as a zero match: {body}"
        );
        if target.starts_with("/api/") {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["status"], serde_json::json!(400), "{target}: {body}");
            assert!(v["matched"].is_null(), "{target}: {body}");
            assert!(
                v["note"].as_str().unwrap().contains("not an empty result"),
                "{target}: {body}"
            );
        }
    }
    // An unresolvable filter and an unresolvable page are the same class of
    // refusal, and the vocabulary error names the vocabulary.
    let (_, body) = ui.get("/sessions?sort=by-time");
    assert!(body.contains("last-desc"), "{body}");
    // When both are wrong the filter answers first: one refusal names one
    // thing, and neither is ever an empty list.
    let (status, body) = ui.get("/sessions?day=not-a-date&limit=0");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("calendar date"), "{body}");
    assert!(!body.contains("`limit`"), "one refusal at a time: {body}");
}

#[test]
fn an_offset_past_the_list_is_an_empty_window_not_a_zero_hit() {
    let sb = sandbox();
    let (repo, key) = build_page_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // The API: 200, empty rows, the matched count intact, the offset echoed
    // rather than silently clamped.
    let (status, body) = ui.get("/api/sessions?offset=9999&limit=3");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["matched"], serde_json::json!(10), "{body}");
    assert_eq!(rows_of(&body).len(), 0, "{body}");
    assert_eq!(
        v["paging"],
        serde_json::json!({
            "total": 10, "limit": 3, "offset": 9999, "sort": "last-desc"
        }),
        "{body}"
    );

    // The page: its own sentence, the way back, and none of the three
    // "nothing matched" words.
    let (status, html) = ui.get("/sessions?offset=9999");
    assert_eq!(status, 200);
    assert!(html.contains("No rows on this page."), "{html}");
    assert!(html.contains("Back to page 1"), "{html}");
    assert!(!html.contains("Not in this destination"), "{html}");

    // While a genuine zero match keeps its own three-state sentence with the
    // paging parameters riding along: a windowed zero is still the honest
    // "not in this destination", and cannot become "No rows on this page".
    let (status, html) = ui.get("/sessions?machine=mbp-zz&offset=99&limit=500&sort=size-desc");
    assert_eq!(status, 200);
    assert!(html.contains("Not in this destination"), "{html}");
    assert!(!html.contains("No rows on this page."), "{html}");
}

/// The largest window start a URL can carry is `usize::MAX`, and it is not an
/// error: `offset` is deliberately unclamped, so it must land on the same 200
/// empty window as any other offset past the end. The paging nav above that
/// sentence is the part with teeth — the page number it prints is derived
/// arithmetic, and the largest offset is exactly where derived arithmetic
/// stops being arithmetic. The verdict is the *next* request: a server that
/// died computing this page could not answer it.
#[test]
fn the_largest_offset_a_url_can_carry_is_still_an_empty_window() {
    let sb = sandbox();
    let (repo, key) = build_page_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // Width 1 makes the page count the row count, so the page numbers below
    // are the archive's own ten; the offset is past every one of them.
    let (status, html) = ui.get(&format!("/sessions?limit=1&offset={}", usize::MAX));
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("No rows on this page."), "{html}");
    assert!(html.contains("Back to page 1"), "{html}");

    // The nav pins the window to the last page. The alternative a wrapped
    // quotient produces is a page 0 no list has, or no current page at all —
    // both would be a number this archive never contained.
    assert_eq!(
        html.matches("aria-current=\"page\"").count(),
        1,
        "exactly one page is current: {html}"
    );
    assert!(
        html.contains("<b aria-current=\"page\">10</b>"),
        "the empty window is pinned to the last page: {html}"
    );

    // The API answers the same offset the same way, echoing it rather than
    // clamping it behind the caller's back.
    let (status, body) = ui.get(&format!("/api/sessions?limit=1&offset={}", usize::MAX));
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["matched"], serde_json::json!(10), "{body}");
    assert_eq!(rows_of(&body).len(), 0, "{body}");
    assert_eq!(
        v["paging"],
        serde_json::json!({
            "total": 10, "limit": 1, "offset": usize::MAX, "sort": "last-desc"
        }),
        "{body}"
    );

    // Reaching this line at all is the proof: the route returned instead of
    // taking the accept loop down with it, so the next request is answered.
    let (status, _) = ui.get("/sessions");
    assert_eq!(status, 200);
}

#[test]
fn walking_the_pages_concatenates_the_whole_list_in_that_walks_order() {
    let sb = sandbox();
    let (repo, key) = build_page_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    // The corpus's one shapable fact: day `d`'s session carries conversation
    // time 2025-03-0d 12:00 UTC, and the deepseek row carries none. Time
    // orders are therefore fully derivable: descending days, then the one
    // unrankable row at the bottom.
    let day_unix = |day: u32| {
        chrono::DateTime::parse_from_rfc3339(&format!("2025-03-{day:02}T12:00:00Z"))
            .unwrap()
            .timestamp()
    };
    for sort in [
        "default",
        "last-desc",
        "last-asc",
        "first-desc",
        "first-asc",
        "size-desc",
    ] {
        // One window no `limit` in the vocabulary can exceed: the clamp is
        // 500 and the archive is 10 rows, so this response holds the whole
        // order the walk below must reproduce.
        let (status, full) = ui.get(&format!("/api/sessions?limit=500&sort={sort}"));
        assert_eq!(status, 200);
        let whole = rows_of(&full);
        assert_eq!(whole.len(), 10, "{sort}: {full}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&full).unwrap()["paging"]["limit"],
            serde_json::json!(500),
            "{sort}: the clamp, not the ask"
        );

        // The walk: pages of 2, 3 and 4 rows — including widths that do not
        // divide 10 — must reassemble into exactly that sequence.
        for limit in [2, 3, 4, 500] {
            let mut walked: Vec<(String, serde_json::Value)> = Vec::new();
            let mut pages_seen = 0;
            for page in 0.. {
                // Bound the walk the contract itself implies: the first empty
                // window is not past `total + limit`. Without the bound, a
                // server that answered every window with the whole list would
                // hang this test instead of failing it.
                assert!(
                    page * limit <= 10 + limit,
                    "{sort}/{limit}: page {} never ran out of rows",
                    page + 1
                );
                let (status, body) = ui.get(&format!(
                    "/api/sessions?limit={limit}&offset={}&sort={sort}",
                    page * limit
                ));
                assert_eq!(status, 200, "{sort}/{limit}: {body}");
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["paging"]["total"], serde_json::json!(10), "{body}");
                assert_eq!(v["matched"], serde_json::json!(10), "{body}");
                assert_eq!(v["not_matched"], serde_json::json!(0), "{body}");
                let page_rows = rows_of(&body);
                if page_rows.is_empty() {
                    // The first empty window starts at or after the list's
                    // end, and the page before it reached into the list — a
                    // walk that stopped early would break both.
                    assert!(
                        page * limit >= 10,
                        "{sort}/{limit}: an empty window before the end: {body}"
                    );
                    assert!(
                        (page - 1) * limit < 10,
                        "{sort}/{limit}: the walk skipped a row: {body}"
                    );
                    break;
                }
                pages_seen += 1;
                walked.extend(page_rows);
            }
            assert_eq!(walked, whole, "{sort} in pages of {limit}");
            // 10 rows: pages of 2 and 3 end mid-sequence; the count is right.
            let expected_pages = (10 + limit - 1) / limit;
            assert_eq!(pages_seen, expected_pages, "{sort}/{limit}");
        }

        // The known time orders: days descending (then the unrankable
        // deepseek row), or ascending with the same row still last.
        let last_col: Vec<&str> = whole
            .iter()
            .map(|(_, t)| t["kind"].as_str().unwrap())
            .collect();
        match sort {
            "last-desc" => {
                for w in whole.iter().take(PAGE_DAYS as usize) {
                    let unix = w.1["unix"].as_i64().unwrap();
                    assert!(
                        (1..=PAGE_DAYS).any(|d| day_unix(d) == unix),
                        "{sort}: {full}"
                    );
                }
                assert_eq!(last_col[9], "unknown", "{sort}: {full}");
                for pair in whole.windows(2).take(PAGE_DAYS as usize - 1) {
                    assert!(
                        pair[0].1["unix"].as_i64().unwrap() > pair[1].1["unix"].as_i64().unwrap(),
                        "{sort} is descending on conversation time: {full}"
                    );
                }
            }
            "last-asc" => {
                assert_eq!(last_col[9], "unknown", "{sort}: {full}");
                for pair in whole.windows(2).take(PAGE_DAYS as usize - 1) {
                    assert!(
                        pair[0].1["unix"].as_i64().unwrap() < pair[1].1["unix"].as_i64().unwrap(),
                        "{sort} is ascending on conversation time: {full}"
                    );
                }
            }
            _ => {}
        }
    }

    // The HTML list and the JSON rows cut the same window of the same order:
    // the JSON page's short ids are exactly the HTML page's short ids, in the
    // same positions.
    let json_row_ids = |body: &str| -> Vec<String> {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["session_short_id"].as_str().unwrap().to_string())
            .collect()
    };
    let html_row_ids = |html: &str| -> Vec<String> {
        html.split("<a class=mono href=\"/session?i=")
            .skip(1)
            .map(|rest| {
                rest.split("\">")
                    .nth(1)
                    .unwrap()
                    .split("</a>")
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    for (limit, offset) in [(4, 0), (4, 6), (500, 0)] {
        let (_, json_body) = ui.get(&format!(
            "/api/sessions?sort=last-asc&limit={limit}&offset={offset}"
        ));
        let (_, html) = ui.get(&format!(
            "/sessions?sort=last-asc&limit={limit}&offset={offset}"
        ));
        let from_json = json_row_ids(&json_body);
        let from_html = html_row_ids(&html);
        assert_eq!(
            from_json, from_html,
            "the HTML list and the API rows are the same window"
        );
        assert_eq!(
            from_json.len(),
            (offset..10).take(limit).count(),
            "the window holds exactly the remaining rows"
        );
    }
}

/// The zero-JS nav is a set of links a browser can actually follow: the next
/// link carries the sort, width and token, and the window it names is the
/// window that renders.
#[test]
fn the_html_nav_links_walk_the_same_list_the_api_does() {
    let sb = sandbox();
    let (repo, key) = build_page_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/sessions?limit=4&sort=last-asc");
    assert_eq!(status, 200);
    assert!(
        html.contains("Sessions 1–4 of 10 · sorted by last message time, oldest first"),
        "the range sentence names the window and the order: {html}"
    );
    assert!(
        html.contains(
            "sessions with an unknown conversation time are not ranked and stay \
                       at the bottom"
        ),
        "the unrankable row is not quietly called the oldest: {html}"
    );
    assert!(
        html.contains("<b aria-current=\"page\">1</b>"),
        "the current page is where the reader already is: {html}"
    );
    // The next link: found in the nav, its target followed, continuation of
    // the list rendered. The token rides in the link itself.
    let target = {
        let frag = html.split("<a href=\"").find(|f| f.contains("next ›"));
        assert!(frag.is_some(), "there is a next link: {html}");
        let frag = frag.unwrap();
        frag[..frag.find('"').unwrap()].to_string()
    };
    for carried in ["limit=4", "sort=last-asc", "offset=4", "token="] {
        assert!(
            target.contains(carried),
            "the link carries {carried}: {target}"
        );
    }
    // Follow it exactly as a browser would.
    let followed = target.split("&token=").next().unwrap().to_string();
    let (status, page2) = ui.get(&followed);
    assert_eq!(status, 200, "{followed}");
    assert!(page2.contains("Sessions 5–8 of 10"), "{followed}: {page2}");
    assert!(page2.contains("‹ previous"), "{page2}");
    assert!(page2.contains("<b aria-current=\"page\">2</b>"), "{page2}");
}

/// No page of any sort can leave the launch filter: the launch filter decides
/// the list once, before the socket is bound, and paging is a window of that
/// decision.
#[test]
fn paging_cannot_page_out_of_the_launch_filter() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &["--machine", "mbp-a"], "ui");

    for sort in ["last-desc", "first-asc", "size-desc", "default"] {
        let mut machines: std::collections::BTreeSet<String> = Default::default();
        let mut walked = 0;
        for page in 0.. {
            // Same bound as the walk test: the filtered list is 2 rows, so an
            // un-ending page sequence is a violation to report, not to keep
            // walking.
            assert!(
                page <= 2 + 1,
                "{sort}: page {} never ran out of rows",
                page + 1
            );
            let (status, body) = ui.get(&format!(
                "/api/sessions?limit=1&offset={}&sort={sort}",
                page
            ));
            assert_eq!(status, 200, "{sort}: {body}");
            let rows = rows_of(&body);
            if rows.is_empty() {
                break;
            }
            for (machine, _) in &rows {
                machines.insert(machine.clone());
            }
            walked += rows.len();
        }
        assert_eq!(walked, 2, "{sort}: the launch filter's list, all of it");
        assert_eq!(
            machines,
            std::collections::BTreeSet::from(["mbp-a".to_string()]),
            "{sort}: no drill-down page can include another machine"
        );
    }
    // An offset the launch filter's list cannot fill is the empty window —
    // with the matched total of the *filtered* list, not a 404 the reader
    // could mistake for a filter having hidden the rows.
    let (status, body) = ui.get("/api/sessions?offset=500&sort=size-desc");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["matched"], serde_json::json!(2), "{body}");
    assert_eq!(v["paging"]["total"], serde_json::json!(2), "{body}");
    assert_eq!(rows_of(&body).len(), 0, "{body}");
    // The drill-down filter composes with paging as a conjunction: mbp-b's
    // rows are another machine's, the launch filter refuses the whole
    // dimension, and a window of that is still empty — not leaked.
    let (status, body) = ui.get("/api/sessions?machine=mbp-b&offset=0&limit=500");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        v["matched"],
        serde_json::json!(2),
        "sanity: launch standing"
    );
    let v2: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v2["matched"], serde_json::json!(0), "{body}");
    assert_eq!(rows_of(&body).len(), 0, "{body}");
}

// -------------------------------------------------- UIA-3 · platform groups
//
// The grouping map, its facet bar and its JSON column over a real loopback
// socket and real binary. The fixture is the shape the task spec names: one
// machine holding a coding agent session, a web-platform session, a session
// from a platform id NO build classifies (`omega-web` — the "new platform
// lands in ungrouped" case), and one id with no harness prefix at all.

/// A platform id no release of this tool has ever classified. It arrives in
/// the archive exactly the way a real extension platform would: as the
/// leading segment of an extension-delivered session id.
const GROUP_NEW_PLATFORM: &str = "omega-web.grp-0002";
const GROUP_AGENT: &str = "claude-code.mbp-grp.019bf00d-97b6-7eb2-9bf8-eacbacc09871";
const GROUP_WEB: &str = "deepseek.grp-0001";
const GROUP_NO_PREFIX: &str = ".no-prefix-grp";

/// One machine, one snapshot, four sessions — one per platform-group state
/// the facet bar counts, all synthetic.
fn build_grouped_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let pairs = [
        (GROUP_AGENT, vec![cc_line("2025-04-01T12:00:00Z")]),
        (GROUP_WEB, vec![ext_line()]),
        (GROUP_NEW_PLATFORM, vec![ext_line()]),
        // `.no-prefix-grp` renders `(no harness prefix)`: nothing before its
        // leading `.`, so `infer_harness` has no head to return.
        (GROUP_NO_PREFIX, vec![cc_line("2025-04-02T09:30:00Z")]),
    ];
    let stage = stage_for(sandbox, "mbp-grp", &pairs);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let indexed = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-grp",
        ],
    );
    assert!(indexed.status.success(), "activity-index: {indexed:?}");
    let pushed = run(
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
            "mbp-grp",
            "--keep-ssh-masters",
        ],
    );
    assert!(
        pushed.status.success(),
        "push failed: {:?}\n{}",
        pushed.status,
        String::from_utf8_lossy(&pushed.stderr)
    );
    (repo, key)
}

/// The facet bar's link cells as `(href, label)` pairs. The nav block is
/// rendered by the server itself in one stable shape, so simple string
/// splitting reads the links back without a parser.
fn facet_bar_links(html: &str) -> Vec<(String, String)> {
    let start = html
        .find("<nav class=sub aria-label=\"platform groups\">")
        .expect("the platform-group bar must be on the page");
    let bar = &html[start..];
    let end = bar.find("</nav>").expect("the bar must close");
    let bar = &bar[..end];
    let mut out = Vec::new();
    let mut rest = bar;
    while let Some(at) = rest.find("<a href=\"") {
        let after = &rest[at + 9..];
        let (href, tail) = after
            .split_once("\">")
            .unwrap_or_else(|| panic!("a facet cell without its label in {bar}"));
        let label = tail
            .split_once("</a>")
            .unwrap_or_else(|| panic!("a facet cell that never closes in {bar}"))
            .0;
        out.push((href.to_string(), label.to_string()));
        rest = &after[href.len()..];
    }
    out
}

/// **The pin, both directions.** Every facet-bar link is a plain `--harness`
/// list the shared selector can read: turning the link's query back into
/// command-line flags and running `search` must return the same sessions the
/// dashboard's own `/api/sessions` returns — including the "counts" that are
/// not match results (no-prefix rows appear as `could_not_be_placed` on both
/// sides, never as absent). And the count a link advertises is the matched
/// count its own page reports, so the bar can never promise a set the click
/// does not show.
#[test]
fn facet_bar_links_are_the_shared_selector_and_their_counts_do_not_lie() {
    let sb = sandbox();
    let (repo, key) = build_grouped_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200, "{html}");
    // The bar names the fixture's hand-counted groups: All 4, one web
    // platform, one coding agent, one unclassified id — with All current.
    assert!(
        html.contains("<b aria-current=\"true\">All 4</b>"),
        "the bar's All count is the four sessions: {html}"
    );
    assert!(
        html.contains("Sources this build does not classify sit under <i>ungrouped</i>"),
        "the bar explains the ungrouped bucket: {html}"
    );
    let links = facet_bar_links(&html);
    assert_eq!(links.len(), 3, "web, agents, ungrouped: {html}");
    let ungrouped = links
        .iter()
        .find(|(href, _)| href.contains("omega-web"))
        .unwrap_or_else(|| panic!("the ungrouped link must name the unclassified id: {links:?}"));
    assert_eq!(ungrouped.1, "ungrouped 1", "the bar counts it honestly");

    for (href, label) in &links {
        // 1. The advertised count is the matched count of the link's own page.
        let query = href
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .to_string();
        let without_token: Vec<&str> = query
            .split('&')
            .filter(|kv| !kv.starts_with("token="))
            .collect();
        let (status, body) = ui.get(&format!("/api/sessions?{}", without_token.join("&")));
        assert_eq!(status, 200, "{href}: {body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        let advertised: u64 = label
            .rsplit_once(' ')
            .and_then(|(_, n)| n.parse().ok())
            .unwrap_or_else(|| panic!("`{label}` carries a count"));
        let matched = page["matched"].as_u64().unwrap();
        assert_eq!(
            matched, advertised,
            "`{href}` matched {matched}, the bar advertised {advertised}"
        );

        // 2. The same query as command-line flags is the same list — the
        // same equality `a_drill_down_returns_exactly_what_search_returns`
        // pins for hand-written queries, now for the ones the server itself
        // emits, multi-value `--harness` lists included.
        let mut args: Vec<String> = vec!["search".into(), "--json".into()];
        args.push("--repo".into());
        args.push(repo.to_str().unwrap().into());
        args.push("--key-file".into());
        args.push(key.to_str().unwrap().into());
        args.push("--keep-ssh-masters".into());
        for kv in &without_token {
            let (k, v) = kv.split_once('=').unwrap();
            assert_ne!(k, "token", "the token is not a selector flag");
            args.push(format!("--{k}"));
            args.push((*v).to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(sb.path(), &arg_refs);
        let searched = first_json(&out.stdout);
        let from_ui: Vec<String> = page["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_short_id"].as_str().unwrap().to_string())
            .collect();
        let from_search: Vec<String> = searched["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_short_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            from_ui, from_search,
            "`{href}`: the dashboard list and search disagree"
        );
        // And the non-matches agree too — a group link's no-prefix rows are
        // `could_not_be_placed` on both sides, never silently dropped.
        assert_eq!(
            page["could_not_be_placed"].as_u64().unwrap_or(0),
            searched["could_not_be_placed"].as_u64().unwrap_or(0),
            "`{href}`: the two sides disagree about the unplaceable rows"
        );
        assert_eq!(
            page["not_matched"].as_u64().unwrap_or(0),
            searched["not_matched"].as_u64().unwrap_or(0),
            "`{href}`"
        );
    }

    // Following the ungrouped link (a new platform's own filter, no release
    // needed) is the one session it promises — and the no-prefix session is
    // listed as could-not-be-placed there, with the reason it can never be
    // evaluated against.
    let (status, body) = ui.get("/api/sessions?harness=omega-web");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["matched"], serde_json::json!(1), "{body}");
    assert_eq!(v["could_not_be_placed"], serde_json::json!(1), "{body}");
    assert!(
        body.contains("archived id carries no harness prefix"),
        "the unplaceable reason must name the lack of a prefix: {body}"
    );
}

/// Platform groups reach both surfaces on the real server: the overview's
/// matrix columns grouped and ordered web → agents → ungrouped → no-prefix,
/// and `/api/sessions` rows carrying the `platform_group` a consumer filters
/// on — `ungrouped` for the new platform, `null` for the no-prefix row.
#[test]
fn platform_groups_reach_the_matrix_and_the_json_rows() {
    let sb = sandbox();
    let (repo, key) = build_grouped_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/");
    assert_eq!(status, 200, "{html}");
    let matrix = html
        .split("<h2>Machine × source</h2>")
        .nth(1)
        .and_then(|rest| rest.split("</section>").next())
        .expect("the matrix section");
    assert!(
        matrix.contains("<th colspan=1 class=\"ghead g-web\">Web platforms</th>"),
        "one web platform, one group cell: {matrix}"
    );
    assert!(
        matrix.contains("<th colspan=1 class=\"ghead g-agents\">Coding agents</th>"),
        "{matrix}"
    );
    assert!(
        matrix.contains("<th colspan=1 class=\"ghead g-ungrouped\">ungrouped</th>"),
        "the unclassified platform is grouped as itself: {matrix}"
    );
    assert!(
        matrix.contains("<th rowspan=2 class=n>(no harness prefix)</th>"),
        "the no-prefix column is its own spanning header, not a group: {matrix}"
    );
    let order: Vec<usize> = ["deepseek", "claude-code", "omega-web"]
        .iter()
        .map(|label| {
            matrix
                .find(&format!(">{}</th>", label))
                .unwrap_or_else(|| panic!("`{label}` must be a matrix header: {matrix}"))
        })
        .collect();
    let mut ascending = order.clone();
    ascending.sort();
    assert_eq!(
        order, ascending,
        "web platforms, then agents, then ungrouped — in that header row"
    );
    // The ungrouped id's cell is a real link: a single unclassified id is a
    // perfectly ordinary one-value harness filter.
    assert!(
        matrix.contains("harness=omega-web&"),
        "the new platform's own cell filters on its own id: {matrix}"
    );

    let (status, body) = ui.get("/api/sessions");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = v["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{body}");
    for row in rows {
        let source = row["source"].as_str().unwrap();
        // The wire words of UIA-3's grouping map, plus its null rule: a row
        // with no harness carries no group either — never a nearest guess.
        let expect = match source {
            "claude-code" => Some("coding-agents"),
            "deepseek" => Some("web-platforms"),
            "omega-web" => Some("ungrouped"),
            "(no harness prefix)" => None,
            other => panic!("unexpected source `{other}`: {body}"),
        };
        match expect {
            Some(word) => {
                assert_eq!(
                    row["platform_group"],
                    serde_json::json!(word),
                    "`{source}`: {body}"
                );
                assert!(!row["harness"].is_null(), "`{source}`: {body}");
            }
            None => {
                assert!(
                    row["harness"].is_null() && row["platform_group"].is_null(),
                    "no harness ⇒ no group, never a guess: {row}"
                );
            }
        }
    }
}

/// One synthetic claude-code turn whose text is given, so a search has real
/// words to match rather than the fixture's two-character "hi".
fn cc_turn(text: &str, ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// The search page against an index the **real** command built.
///
/// This is the half of `/search` no unit test can reach: the identity the
/// server derives for a `--repo` launch has to be the one `index build` wrote,
/// the index has to be read from the cache directory the OS gives the sandbox,
/// and the whole chain — archived JSONL → extracted text → per-message offsets
/// → a message number → a `/reader#m<n>` anchor — has to come out the other end
/// with the conversation's own characters escaped on the way.
#[test]
fn the_search_page_reads_the_index_the_cli_built() {
    const SESSION: &str = "claude-code.mbp-s.019bf00d-97b6-7eb2-9bf8-eacbacc09799";
    let sb = sandbox();
    let sandbox = sb.path();
    let stage = stage_for(
        sandbox,
        "mbp-s",
        &[(
            SESSION,
            vec![
                cc_turn(
                    "the first turn says nothing in particular",
                    "2025-03-01T00:00:00Z",
                ),
                cc_turn(
                    // Conversation text carrying markup and an ampersand, so
                    // the escaping on the way to the page is exercised by a
                    // real indexed document rather than by a fixture string.
                    "a <script>alert(1)</script> & turn mentions hedgehogs",
                    "2025-03-01T00:01:00Z",
                ),
            ],
        )],
    );
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let run_ok = |args: &[&str]| {
        let out = run(sandbox, args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_ok(&[
        "activity-index",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "mbp-s",
    ]);
    run_ok(&[
        "push",
        "--stage",
        stage.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--key-file",
        key.to_str().unwrap(),
        "--machine",
        "mbp-s",
        "--keep-ssh-masters",
    ]);

    let ui = Ui::start(sandbox, &repo, &key, &[], "ui");

    // Before any index exists: the page says so, names the command that builds
    // one, and offers no count that could be read as a result.
    let (status, body) = ui.get("/search?q=hedgehogs");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("No index."), "{body}");
    assert!(body.contains("chat-stasher index build"), "{body}");
    assert!(
        !body.contains("Not in the indexed archive"),
        "an unbuilt index must never render as a proven absence: {body}"
    );

    // The real command, and the running server picks it up: no restart, which
    // is also what a user does after being told to build one.
    run_ok(&[
        "index",
        "build",
        "--repo",
        repo.to_str().unwrap(),
        "--key-file",
        key.to_str().unwrap(),
    ]);

    let (status, body) = ui.get("/search?q=hedgehogs");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("<mark>hedgehogs</mark>"),
        "the matched span must be marked in the excerpt: {body}"
    );
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; turn"),
        "the conversation's own characters must be escaped: {body}"
    );
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "conversation text must never reach the page as markup: {body}"
    );
    assert!(
        body.contains("#m1"),
        "the match is in the second message, so the hit must anchor to message 1: {body}"
    );
    assert!(
        body.contains("/reader?i=0&m=0&n=50"),
        "the anchor must sit in the window the reader opens by default: {body}"
    );
    assert!(
        body.contains("index coverage: <b>1</b> of <b>1</b> session(s)"),
        "a complete index over the archived session must say so: {body}"
    );

    // The JSON body answers the same question the same way, and names the
    // message it anchored to rather than leaving a consumer to parse the href.
    let (status, body) = ui.get("/api/search?q=hedgehogs");
    assert_eq!(status, 200, "{body}");
    let value = first_json(body.as_bytes());
    assert_eq!(
        value["query_state"],
        serde_json::json!("answered"),
        "{body}"
    );
    assert!(value["no_hit"].is_null(), "{body}");
    assert_eq!(value["mode"], serde_json::json!("fts"), "{body}");
    assert_eq!(value["matched"], serde_json::json!(1), "{body}");
    assert_eq!(
        value["results"][0]["matched_in"],
        serde_json::json!("message")
    );
    assert_eq!(
        value["results"][0]["message_ordinal"],
        serde_json::json!(2),
        "message numbers are 1-based for a reader, and the index's are 0-based: {body}"
    );
    assert!(
        value["results"][0]["snippet"]
            .as_array()
            .is_some_and(|segments| segments
                .iter()
                .any(|s| s["matched"] == serde_json::json!(true))),
        "the excerpt must carry which runs matched: {body}"
    );

    // A text the archive really does not hold, over a complete index and a
    // complete read: this is the one case that earns an absence.
    let (status, body) = ui.get("/search?q=definitelynotinthearchive");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Not in the indexed archive"), "{body}");

    // …and one character fewer than the tokenizer can answer is neither a
    // result nor an absence.
    let (status, body) = ui.get("/search?q=ab");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("This query cannot be evaluated"), "{body}");
}

/// A query that cannot resolve is refused on the search routes exactly as it is
/// on the list ones, in both wire forms — never as a page of zero results.
#[test]
fn the_search_routes_refuse_an_unresolvable_parameter() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    for (target, needle) in [
        ("/search?q=hi&day=yesterday", "day"),
        ("/search?q=hi&limit=0", "limit"),
        ("/search?q=hi&sort=size-desc", "sort"),
    ] {
        let (status, body) = ui.get(target);
        assert_eq!(status, 400, "{target}: {body}");
        assert!(body.contains(needle), "{target}: {body}");
    }
    for (target, needle) in [
        ("/api/search?q=hi&day=yesterday", "day"),
        ("/api/search?q=hi&limit=0", "limit"),
        ("/api/search?q=hi&sort=size-desc", "sort"),
    ] {
        let (status, body) = ui.get(target);
        assert_eq!(status, 400, "{target}: {body}");
        let value = first_json(body.as_bytes());
        assert_eq!(value["status"], serde_json::json!(400), "{target}: {body}");
        assert_eq!(value["query_state"], serde_json::json!("refused"), "{body}");
        assert!(
            value["note"]
                .as_str()
                .is_some_and(|note| note.contains("not an empty result")),
            "{target} must not answer a refusal with something a consumer reads as empty: {body}"
        );
        let _ = needle;
    }
}

// ------------------------------------------------------------- R7 /export

/// One GET that preserves raw bytes — the download's body is the session's
/// exact shard bytes and a lossy decode would hide exactly the class of
/// difference this suite exists to catch. Returns the status, the whole head
/// (the named headers are asserted against it) and the undecoded body.
fn http_raw(port: u16, target: &str) -> (u16, String, Vec<u8>) {
    use std::io::Write as _;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect loopback");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?}"));
    (status, head, raw[split + 4..].to_vec())
}

fn head_value<'a>(head: &'a str, name: &str) -> &'a str {
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
        })
        .unwrap_or_else(|| panic!("no {name} header in {head}"))
}

/// The row index the dashboard addresses a session by, found through the same
/// `/api/sessions` a consumer walks — the raw session id never reaches a URL.
fn row_index(ui: &Ui, machine: &str, session_id: &str) -> usize {
    let (status, body) = ui.get("/api/sessions?limit=500");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = v["sessions"].as_array().unwrap();
    let short = chat_stasher::id::short_session_id(session_id);
    let row = rows
        .iter()
        .find(|r| {
            r["machine"].as_str() == Some(machine)
                && r["session_short_id"].as_str() == Some(short.as_str())
        })
        .unwrap_or_else(|| panic!("machine {machine} session {short} missing from {body}"));
    row["index"].as_u64().unwrap() as usize
}

/// The headline of R7 (29-UI-DESIGN §4.7): the download's bytes are exactly
/// the session's archived lines — the same bytes the stage pushed, `read`
/// reads back and the `export` CLI writes — with the digest `read` prints
/// riding the response as `X-Checksum-Sha256`.
#[test]
fn ui_export_downloads_the_exact_read_bytes_as_an_attachment() {
    use sha2::{Digest, Sha256};
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let index = row_index(&ui, "mbp-a", WIDE);

    // The exact oracle bytes: the stage file `push` sealed into the archive,
    // restored untouched ("not re-encoded, not re-serialised, not
    // re-terminated" is export's own contract, and the download shares the
    // fetch path that guarantees it).
    let sealed = sb
        .path()
        .join("stage-mbp-a")
        .join("sessions")
        .join("mbp-a")
        .join(WIDE)
        .join("000")
        .join("000001.jsonl");
    let sealed = fs::read(&sealed).unwrap();
    let digest = Sha256::digest(&sealed)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let short = chat_stasher::id::short_session_id(WIDE);

    let (status, head, body) = http_raw(
        ui.port,
        &format!("/export?i={index}&fmt=jsonl&token={}", ui.token),
    );
    assert_eq!(status, 200, "{head}");
    assert_eq!(
        head_value(&head, "Content-Disposition"),
        format!("attachment; filename=\"{short}.jsonl\""),
        "an attachment named after the short id — no title, no full id"
    );
    assert_eq!(
        head_value(&head, "X-Checksum-Sha256"),
        digest,
        "the header is the digest of exactly the bytes below it"
    );
    assert_eq!(
        head_value(&head, "Content-Type"),
        "application/octet-stream"
    );
    assert_eq!(
        head_value(&head, "Content-Length"),
        sealed.len().to_string()
    );
    // The core claim, byte for byte: no line moved, nothing was re-encoded.
    assert!(
        body == sealed,
        "downloaded {} bytes, archived {} — they must be identical",
        body.len(),
        sealed.len()
    );

    // The same selector through the CLI: `read` prints the concat digest
    // (the CLI's own output for a session), and those numbers must be these
    // numbers.
    let out = run(
        sb.path(),
        &[
            "read",
            "--stage",
            sb.path().join("stage-mbp-a").to_str().unwrap(),
            "--session",
            WIDE,
            "--machine",
            "mbp-a",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(out.status.success(), "read failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let concat_line = stdout
        .lines()
        .find(|l| l.starts_with("[read] concat len"))
        .expect("read prints the concatenation digest");
    assert!(
        concat_line.contains(&format!("sha256={digest}")),
        "read's own digest must match the response header: {concat_line}"
    );
    assert!(
        concat_line.contains(&format!("{}  sha", sealed.len())),
        "read's own length must match the download: {concat_line}"
    );

    // …and through the `export` CLI, which writes the same bytes to a file
    // (P7: the UI's parity command is the real command, and its bytes are
    // cannot-differ bytes). `--session WIDE` is the same shared selector the
    // download's session is addressed by.
    let out_dir = sb.path().join("export-by-ui");
    let out = run(
        sb.path(),
        &[
            "export",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            "mbp-a",
            "--session",
            WIDE,
            "--out",
            out_dir.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(out.status.success(), "export failed: {out:?}");
    let written = fs::read(
        out_dir
            .join("mbp-a/claude-code")
            .join(format!("{WIDE}.jsonl")),
    )
    .unwrap();
    assert!(
        written == sealed && written == body,
        "export writes the same bytes"
    );
}

/// The footer of /sessions names the CLI twin of the view (29-UI-DESIGN
/// §3.5), and the session page links the single-session download with the
/// digest stated beside it — the command must be the one the page claims:
/// pasteable, and selecting the set the page shows.
#[test]
fn the_sessions_page_prints_the_equivalent_cli_command_and_the_download_link() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    // A filtered view: one machine, one source — the printed flags must be
    // exactly these, in the vocabulary `export` reads.
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");

    let (status, html) = ui.get("/sessions?machine=mbp-a&harness=claude-code");
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<h2>Export this view (CLI)</h2>"),
        "the block must exist: {html}"
    );
    assert!(
        html.contains("chat-stasher export --repo &lt;this dashboard&#39;s repository&gt;")
            || html.contains("chat-stasher export --destination "),
        "vs `--repo`, but the command must name a target: {html}"
    );
    assert!(
        html.contains("--machine &#39;mbp-a&#39; --harness &#39;claude-code&#39; --out ~/out"),
        "the filters this page applied, spelled as quoted flags: {html}"
    );

    // The command is not decoration: it must select the set the page shows.
    // Run it against this repository and compare the written set with the
    // page's own matched count.
    let (status, body) = ui.get("/api/sessions?machine=mbp-a&harness=claude-code");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let matched = v["matched"].as_u64().unwrap();
    let out_dir = sb.path().join("export-parity");
    let out = run(
        sb.path(),
        &[
            "export",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            "mbp-a",
            "--harness",
            "claude-code",
            "--out",
            out_dir.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(out.status.success(), "export failed: {out:?}");
    let exported = fs::read_dir(out_dir.join("mbp-a").join("claude-code"))
        .unwrap()
        .filter_map(|e| e.ok())
        .count();
    // The selector writes one file per session it could read; anything it
    // could not is recorded in the manifest as a failure — but this synthetic
    // archive is fully readable, so the counts must meet exactly.
    assert_eq!(
        exported, matched as usize,
        "the page's command must select the page's set"
    );

    // The session page: the download link exists beside the cost, and the
    // digest statement is on the page.
    let index = row_index(&ui, "mbp-a", WIDE);
    let (status, page) = ui.get(&format!("/session?i={index}"));
    assert_eq!(status, 200, "{page}");
    assert!(
        page.contains(&format!("/export?i={index}&fmt=jsonl&token=")),
        "the row's own download link: {page}"
    );
    assert!(
        page.contains("X-Checksum-Sha256</code>"),
        "the digest statement rides the link: {page}"
    );
}

/// Percent-encode like the server's `percent_encode` (everything outside the
/// unreserved set), so a hostile-value probe rides the URL byte-exact.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Undo the server-side `esc` — what a browser renders before the reader
/// copies it. `&amp;` must go first so an escaped ampersand cannot be
/// re-decoded as a second entity.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// W195b FIX-FIRST: hostile filter values in the URL must reach the footer
/// command as one quoted shell word each, and the block must stay
/// HTML-escaped on the wire. All-platform half of the paste-safety proof:
/// the exact bytes a browser renders. The payloads are inert sentinels; if
/// the renderer ever went back to raw interpolation, these bytes cannot
/// match.
#[test]
fn the_footer_command_quotes_hostile_filter_values_on_the_page() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let machine = "mbp-a; printf PWNED; $(printf SUBST) `printf TICK` 'q' \"d\"\nEnd";
    let (status, html) = ui.get(&format!("/sessions?machine={}", url_encode(machine)));
    assert_eq!(status, 200, "{html}");
    let section = html
        .split("<section id=export-cli>")
        .nth(1)
        .expect("the block exists even for a zero-match view")
        .split("</section>")
        .next()
        .unwrap();
    let pre = section
        .split("<pre>")
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .expect("the block prints the command");
    // Still HTML-escaped: the quoting rides as entities, and no quote travels
    // raw — a raw `'` on this line is the escaping half of the same bug.
    assert!(
        pre.contains("&#39;") && pre.contains("&quot;"),
        "quoting must survive the HTML escaper: {pre}"
    );
    assert!(!pre.contains('\''), "raw quotes must be escaped: {pre}");
    assert!(!pre.contains('"'), "raw quotes must be escaped: {pre}");
    assert_eq!(
        pre,
        r##"chat-stasher export --repo &lt;this dashboard&#39;s repository&gt; --machine &#39;mbp-a; printf PWNED; $(printf SUBST) `printf TICK` &#39;&quot;&#39;&quot;&#39;q&#39;&quot;&#39;&quot;&#39; &quot;d&quot;
End&#39; --out ~/out"##,
        "the exact command a browser renders for a hostile machine filter"
    );
}

/// W195b FIX-FIRST: the real-shell half of the paste proof. A POSIX `sh`
/// reading the exact command the block prints — hostile filter values and
/// all, with the one placeholder the block already tells the reader to fill
/// in filled with this test's own synthetic repository — must reach one
/// `chat-stasher` invocation with the payloads intact as single arguments,
/// and nothing the payloads spelled may run on its own: `sh -n` accepts the
/// line, then a PATH stub prints every argument the shell handed it.
///
/// `#[cfg(unix)]` states a property gap rather than silencing it: parsing a
/// pasted command is defined BY a POSIX shell (XCU §2.2), which does not
/// exist on Windows; the all-platform halves of this proof — the block's
/// exact bytes — run in the test above.
#[cfg(unix)]
#[test]
fn the_footer_command_pastes_into_a_posix_shell_as_one_command() {
    use std::os::unix::fs::PermissionsExt;

    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let machine = "mbp-a; printf PWNED; $(printf SUBST) `printf TICK` 'q' \"d\"\nEnd";
    let (status, html) = ui.get(&format!("/sessions?machine={}", url_encode(machine)));
    assert_eq!(status, 200, "{html}");
    let section = html
        .split("<section id=export-cli>")
        .nth(1)
        .expect("the block exists even for a zero-match view")
        .split("</section>")
        .next()
        .unwrap();
    let pre = section
        .split("<pre>")
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .expect("the block prints the command");
    let command = html_unescape(pre);
    // The one thing the reader must supply: the repository the block refuses
    // to name. Filling it — and nothing else — is the faithful paste.
    let placeholder = "--repo <this dashboard's repository>";
    let repo_flag = format!("--repo {}", repo.to_str().unwrap());
    assert!(
        command.contains(placeholder),
        "the block names the one thing the reader fills in: {command}"
    );
    let pasted = command.replace(placeholder, &repo_flag);

    // Parse, do not execute: a line that parses cleanly cannot have an
    // unterminated quote or live rediression in it.
    let script = sb.path().join("pasted-command.sh");
    fs::write(&script, &pasted).unwrap();
    let parsed = Command::new("sh").arg("-n").arg(&script).output().unwrap();
    assert!(
        parsed.status.success(),
        "the pasted line must parse: {}",
        String::from_utf8_lossy(&parsed.stderr)
    );

    // Execute through a stub that answers to `chat-stasher` and prints the
    // argument list the shell assembled — the paste's argv, on the record.
    let stub_dir = sb.path().join("stubbin");
    fs::create_dir_all(&stub_dir).unwrap();
    let stub = stub_dir.join("chat-stasher");
    fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let home = sb.path().join("home");
    let out = Command::new("sh")
        .arg("-c")
        .arg(&pasted)
        .env(
            "PATH",
            format!(
                "{}:{}",
                stub_dir.to_str().unwrap(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the stubbed paste run failed: {out:?}"
    );
    assert!(
        out.stderr.is_empty(),
        "nothing the payloads spelled may run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = format!(
        "export\n--repo\n{}\n--machine\n{}\n--out\n{}\n",
        repo.to_str().unwrap(),
        machine,
        home.join("out").to_str().unwrap(),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected,
        "one chat-stasher invocation, each payload intact as one argument"
    );
}

/// `fmt` is a vocabulary, not a request: an unknown value is refused, and a
/// refused or unresolvable request fetches nothing (the refusal page is a
/// usage error, and no payload is paid for it).
#[test]
fn the_export_route_refuses_what_it_cannot_serve() {
    let sb = sandbox();
    let (repo, key) = build_repo(sb.path());
    let ui = Ui::start(sb.path(), &repo, &key, &[], "ui");
    let index = row_index(&ui, "mbp-a", WIDE);

    for target in [
        format!("/export?i={index}&fmt=csv&token={}", ui.token),
        format!("/export?i=9999&fmt=jsonl&token={}", ui.token),
        format!("/export?token={}", ui.token),
    ] {
        let (status, head, body) = http_raw(ui.port, &target);
        assert_eq!(status, 400, "{target}: {head}");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("usage error") || text.contains("must be one of"),
            "the refusal must say which vocabulary: {text}"
        );
        assert!(
            !head.contains("Content-Disposition:"),
            "{target} must not be an attachment: {head}"
        );
    }
}
