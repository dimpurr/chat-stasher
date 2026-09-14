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
