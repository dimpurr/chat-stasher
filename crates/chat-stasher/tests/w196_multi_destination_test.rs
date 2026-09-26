//! W196 — `ui --destination a,b`: one dashboard over several destinations.
//!
//! R10 (29-UI-DESIGN §4.8): the per-destination reports are merged **at the row
//! level**, keyed by `(machine, session id)`. Everything here runs the real
//! binary against two real rustic repositories built from synthetic stages, and
//! reads the served pages over a real loopback socket. No machine's archive is
//! touched, and no conversation text is asserted on.
//!
//! Properties pinned (the W196 brief):
//!
//! * A session both destinations hold is **one row** with a `×2 backup` badge,
//!   plus a destination column naming both copies — and the session-count pair
//!   (distinct vs raw) is printed rather than one of them being chosen.
//! * A destination that cannot be read makes the whole page INCOMPLETE, and the
//!   failure names **only** that destination: the copy that read in full must
//!   not be reported as damaged, and its rows must not be dropped to make the
//!   counts agree.
//! * The merged list pages exactly like `/sessions`: walking the offsets and
//!   concatenating reproduces the full list, and a row's `i` handle names the
//!   same session on any page.
//! * The shapes that cannot mean anything are refused as usage errors rather
//!   than guessed: `all` beside a name, a name twice, and the single-repository
//!   flags (`--repo`, `--key-file`, `--option`) alongside several destinations.
//! * One destination is still exactly one destination: no badge, no
//!   destination column, no second count, exit 0.

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
        r#"{"schema_version":1,"generated":"W196 synthetic","harnesses":[]}"#,
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

fn write_config(sandbox: &Path, body: &str) {
    let dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

const T: &str = "2025-01-15T12:00:00Z";

/// Three sessions, so the merged list has more than one page at `limit=2`:
/// `shared` is in both copies, `only-a` and `only-b` in one each.
const SHARED: &str = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
const ONLY_A: &str = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09766";
const ONLY_B: &str = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09767";

fn write_shard(stage: &Path, machine: &str, session: &str, prompt: &str) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    let line = format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"{prompt}"}},"uuid":"u1","timestamp":"{T}","cwd":"/x","version":"1.0.31"}}"#
    );
    fs::write(dir.join("000001.jsonl"), line + "\n").unwrap();
}

/// A destination repository holding `sessions`, plus its masterkey.
fn make_repo(sandbox: &Path, name: &str, sessions: &[&str]) -> (String, String) {
    make_repo_prompted(sandbox, name, sessions, "a synthetic prompt")
}

/// The same, with the prompt every shard is written from.
///
/// The prompt is a parameter because two destinations holding the *same
/// session id* with different prompts hold two different **copies**, and a
/// differing copy is the only thing that makes "reads the copy the row names"
/// observable rather than indistinguishable from reading the other one.
fn make_repo_prompted(
    sandbox: &Path,
    name: &str,
    sessions: &[&str],
    prompt: &str,
) -> (String, String) {
    let stage = sandbox.join(format!("stage-{name}"));
    for session in sessions {
        write_shard(&stage, "mbp-a", session, prompt);
    }
    let repo = sandbox.join(format!("repo-{name}"));
    let key = sandbox.join("keys").join(format!("masterkey-{name}.json"));
    let index = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-a",
        ],
    );
    assert!(index.status.success(), "{index:?}");
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
            "mbp-a",
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "{push:?}");
    (
        repo.to_string_lossy().into_owned(),
        key.to_string_lossy().into_owned(),
    )
}

fn dest_config(repo: &str, key: &str) -> String {
    format!("repo = '{repo}'\nkey_file = '{key}'\n")
}

/// Two destinations: alpha holds `shared` + `only-a`, beta holds `shared` +
/// `only-b`.
fn two_destinations(sandbox: &Path) {
    let (repo_a, key_a) = make_repo(sandbox, "alpha", &[SHARED, ONLY_A]);
    let (repo_b, key_b) = make_repo(sandbox, "beta", &[SHARED, ONLY_B]);
    write_config(
        sandbox,
        &format!(
            "[destinations.alpha]\n{}\n[destinations.beta]\n{}\n",
            dest_config(&repo_a, &key_a),
            dest_config(&repo_b, &key_b),
        ),
    );
}

/// The same two destinations, except `shared` is sealed from a **different**
/// prompt in each — so the two copies of that one session id are different
/// bytes, and a payload read can be attributed to the copy it came from.
fn two_destinations_with_differing_copies(sandbox: &Path) {
    let (repo_a, key_a) = make_repo_prompted(sandbox, "alpha", &[SHARED, ONLY_A], "alpha's copy");
    let (repo_b, key_b) = make_repo_prompted(sandbox, "beta", &[SHARED, ONLY_B], "beta's copy");
    write_config(
        sandbox,
        &format!(
            "[destinations.alpha]\n{}\n[destinations.beta]\n{}\n",
            dest_config(&repo_a, &key_a),
            dest_config(&repo_b, &key_b),
        ),
    );
}

/// The bytes `push` sealed for one session in one destination's stage — what
/// `read` returns for it, and therefore what `/export` must hand back.
fn sealed_bytes(sandbox: &Path, destination: &str, session: &str) -> Vec<u8> {
    fs::read(
        sandbox
            .join(format!("stage-{destination}"))
            .join("sessions")
            .join("mbp-a")
            .join(session)
            .join("000")
            .join("000001.jsonl"),
    )
    .expect("the stage file the archive was sealed from")
}

// ----------------------------------------------------------------- socket

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
            // receive buffer still held data; the Content-Length check below is
            // what proves nothing was lost.
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

/// A running `ui` server started **from the config**, with no `--repo`.
struct Ui {
    child: Child,
    port: u16,
    token: String,
}

impl Ui {
    /// Start `chat-stasher ui` with `extra` flags and read its stdout until the
    /// URL appears. `stdout` is also kept, because the narration is half of what
    /// this command promises.
    fn start(sandbox: &Path, extra: &[&str]) -> (Ui, String) {
        let home = sandbox.join("home");
        let registry = sandbox.join("registry.json");
        fs::create_dir_all(&home).unwrap();
        let stderr_path = sandbox.join("ui.stderr");
        let stderr = fs::File::create(&stderr_path).unwrap();
        let mut args: Vec<String> = vec![
            "ui".into(),
            "--no-open".into(),
            "--idle-timeout".into(),
            "15".into(),
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
        let mut narration = String::new();
        let mut url = None;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            narration.push_str(&line);
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("http://") {
                url = Some(rest.to_string());
                break;
            }
            line.clear();
        }
        // The rest of the narration (everything after the URL) is read on drop
        // and is not needed: every assertion below is about the lines above it.
        let url = url.unwrap_or_else(|| {
            panic!(
                "ui never printed a URL; narration:\n{narration}\nstderr:\n{}",
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
        (Ui { child, port, token }, narration)
    }

    fn get(&self, target: &str) -> (u16, String) {
        let sep = if target.contains('?') { '&' } else { '?' };
        http(self.port, &format!("{target}{sep}token={}", self.token))
    }

    /// The session links of a `/sessions` window, in the order they appear —
    /// the `i` handles plus the short ids, which is what "paging" has to agree
    /// about.
    fn rows(&self, query: &str) -> Vec<(String, String)> {
        let (status, html) = self.get(&format!("/sessions{query}"));
        assert_eq!(status, 200, "{query}: {html}");
        links_of(&html)
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
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

/// Every `(i, short id)` pair in a `/sessions` body, in page order.
fn links_of(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("href=\"/session?i=") {
        rest = &rest[at + "href=\"/session?i=".len()..];
        let Some((index, tail)) = rest.split_once('"') else {
            break;
        };
        let index = index.split('&').next().unwrap_or(index).to_string();
        let Some((_, after)) = tail.split_once('>') else {
            break;
        };
        let Some((short_id, _)) = after.split_once("</a>") else {
            break;
        };
        out.push((index, short_id.to_string()));
    }
    out
}

// ------------------------------------------------------------------- tests

/// The merge itself: one row for the session both copies hold, the badge, the
/// destination column, and both counts on the page and on stdout.
#[test]
fn two_destinations_merge_row_by_row_with_the_badge_and_both_counts() {
    let sb = sandbox();
    two_destinations(sb.path());
    let (ui, narration) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);

    for expected in [
        "[ui] destination  : alpha,beta",
        "[ui] merge order  : alpha,beta",
        "[ui] merged       : 3 distinct session(s) / 4 raw copy(ies) in view",
        "[ui] raw copies   : 4 in the archive across 2 destinations",
        "[ui]   · alpha",
        "[ui]   · beta",
    ] {
        assert!(
            narration.contains(expected),
            "the narration must carry `{expected}`:\n{narration}"
        );
    }

    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    // One row for the shared session, not two, and it says how many copies.
    assert_eq!(
        links_of(&html).len(),
        3,
        "three distinct sessions, though four copies exist"
    );
    assert!(
        html.contains("×2 backup"),
        "the shared session must be badged:\n{html}"
    );
    assert!(
        html.contains("<th>destination</th>"),
        "a merged list has a destination column:\n{html}"
    );
    assert!(
        html.contains("<b>distinct:</b> 3 session(s)"),
        "the distinct count must be printed:\n{html}"
    );
    assert!(
        html.contains("<b>raw:</b> 4 session row(s)"),
        "the raw count must be printed beside it, not chosen over it:\n{html}"
    );
    assert!(
        !html.contains("INCOMPLETE READ."),
        "both copies read in full:\n{html}"
    );
    // Only the shared session is badged: the other two are single copies. The
    // count is taken from the rows' own badge markup, because the block above
    // the table spells the convention out once and would otherwise be counted.
    assert_eq!(
        html.matches("class=badge title=\"held by 2 destinations")
            .count(),
        1,
        "exactly one row is a second copy:\n{html}"
    );

    // The same two counts, and the per-copy breakdown, on the JSON route.
    let (status, body) = ui.get("/api/sessions");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["sessions_distinct"], 3);
    assert_eq!(v["sessions_raw"], 4);
    assert_eq!(v["destinations"][0]["label"], "alpha");
    assert_eq!(v["destinations"][0]["complete"], true);
    assert_eq!(v["destinations"][1]["label"], "beta");
    assert_eq!(v["paging"]["total"], 3);
    let badged: Vec<&serde_json::Value> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["destinations"].as_array().unwrap().len() == 2)
        .collect();
    assert_eq!(badged.len(), 1, "exactly one row is held by both copies");
    assert_eq!(
        badged[0]["destinations"],
        serde_json::json!(["alpha", "beta"])
    );
}

/// The merged list pages exactly like `/sessions` (§5.2): concatenating the
/// windows reproduces the whole list, and a row's `i` handle names the same
/// session on every page — the two halves of "paging did not become a filter".
#[test]
fn the_merged_list_pages_like_sessions_does() {
    let sb = sandbox();
    two_destinations(sb.path());
    let (ui, _) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);

    let whole = ui.rows("?limit=100");
    assert_eq!(whole.len(), 3, "the whole merged list: {whole:?}");
    let first = ui.rows("?limit=2&offset=0");
    let second = ui.rows("?limit=2&offset=2");
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 1);
    let mut walked = first.clone();
    walked.extend(second.clone());
    assert_eq!(
        walked, whole,
        "walking the pages must reproduce the list, in order"
    );

    // The `i` handles come from the same inventory on every window, so a link
    // taken from page 2 resolves to the session it showed there.
    let (index, short_id) = second[0].clone();
    let (status, page) = ui.get(&format!("/session?i={index}"));
    assert_eq!(status, 200, "{page}");
    assert!(
        page.contains(&short_id),
        "`i={index}` from page 2 must resolve to {short_id}:\n{page}"
    );

    // And a window past the end is a paging position, never an absence.
    let (status, past) = ui.get("/sessions?limit=2&offset=9");
    assert_eq!(status, 200);
    assert!(
        past.contains("No rows on this page."),
        "a window past the end is a position, not a count:\n{past}"
    );
    assert!(
        !past.contains("Not in these destinations"),
        "and it must not read as a match count:\n{past}"
    );
}

/// Both counts reach every merged page a reader can get to — the overview, and
/// the list page in the state where its answer is "nothing matched".
///
/// §4.8's pair is a property of the **view**, not of an answer. The overview
/// headlines the distinct reading and the raw one cannot be recovered from it;
/// the zero-match sentence replaces the list's matched line, and the counts
/// used to hang off that line, so the one page that most needs "two copies of
/// one conversation, not two conversations" was the page that never said it.
/// One destination is pinned the other way: one reading, no pair, because
/// there the two numbers are equal by construction.
#[test]
fn every_merged_page_carries_both_counts() {
    let sb = sandbox();
    two_destinations(sb.path());
    let (ui, _) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);

    let (status, overview) = ui.get("/");
    assert_eq!(status, 200);
    assert!(
        overview.contains("<b>distinct:</b> 3 session(s)"),
        "the overview headlines the distinct reading and must say so:\n{overview}"
    );
    assert!(
        overview.contains("<b>raw:</b> 4 session row(s)"),
        "and carry the raw one beside it, not instead of it:\n{overview}"
    );

    // A facet value no session carries: the answer becomes the zero-match
    // sentence, and the pair has to survive the branch that prints it.
    let (status, none) = ui.get("/sessions?machine=no-such-machine");
    assert_eq!(status, 200, "{none}");
    assert!(
        none.contains("session(s) in view matched"),
        "this request must actually reach the zero-match page:\n{none}"
    );
    assert!(
        none.contains("<b>distinct:</b> 3 session(s)"),
        "a zero-match page is still a page of a merged dashboard:\n{none}"
    );
    assert!(
        none.contains("<b>raw:</b> 4 session row(s)"),
        "and must not be the one page that hides the redundancy:\n{none}"
    );

    // One destination: one reading, so neither page prints the pair.
    let one = sandbox();
    two_destinations(one.path());
    let (single, _) = Ui::start(one.path(), &["--destination", "alpha"]);
    for target in ["/", "/sessions?machine=no-such-machine"] {
        let (status, html) = single.get(target);
        assert_eq!(status, 200, "{target}: {html}");
        assert!(
            !html.contains("<b>distinct:</b>") && !html.contains("<b>raw:</b>"),
            "`{target}` has one reading and must not print a pair:\n{html}"
        );
    }
}

/// `--destination all` is the same read as naming every destination, because
/// the order it expands to is the config's own (sorted) order.
#[test]
fn all_expands_to_every_declared_destination() {
    let sb = sandbox();
    two_destinations(sb.path());
    let (ui, narration) = Ui::start(sb.path(), &["--destination", "all"]);
    assert!(
        narration.contains("[ui] destination  : alpha,beta"),
        "`all` must name what it expanded to:\n{narration}"
    );
    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    assert_eq!(links_of(&html).len(), 3);
    assert!(html.contains("×2 backup"), "{html}");
}

/// One destination a user could not read makes the whole page INCOMPLETE and
/// names only that one — and the copy that read in full keeps every row.
///
/// The corruption is a truncated snapshot file: a real shape (an interrupted
/// write), and deterministic, unlike corrupting an object pack — rustic's
/// local cache can satisfy a re-read of a truncated pack, which is why the
/// fixture does not use one.
#[test]
fn one_corrupt_destination_contaminates_the_page_and_names_only_itself() {
    let sb = sandbox();
    two_destinations(sb.path());
    let snapshots = sb.path().join("repo-beta").join("snapshots");
    let snapshot = fs::read_dir(&snapshots)
        .expect("beta has a snapshots dir")
        .next()
        .expect("a snapshot file")
        .unwrap()
        .path();
    fs::write(&snapshot, b"").expect("truncate the snapshot");

    let (ui, narration) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);
    assert!(
        narration.contains("beta") && narration.contains("could not be read at all"),
        "the terminal must say which destination never opened:\n{narration}"
    );

    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200, "the readable copy is still served");
    assert!(
        html.contains("INCOMPLETE READ."),
        "one unreadable copy makes the page a floor:\n{html}"
    );
    assert!(
        html.contains("1 of the 2 destinations could not be read in full"),
        "and says how many:\n{html}"
    );
    let at = html.find("INCOMPLETE READ.").expect("the banner");
    let banner = &html[at..at + 1200];
    assert!(
        banner.contains("<b>beta</b>"),
        "the failing destination must be named:\n{banner}"
    );
    assert!(
        !banner.contains("<b>alpha</b>"),
        "and the destination that read in full must not be:\n{banner}"
    );
    // Alpha's two rows are all still there; the merged list is not emptier
    // because one copy failed.
    assert_eq!(
        links_of(&html).len(),
        2,
        "alpha's rows survive beta's failure:\n{html}"
    );
    assert!(
        html.contains("<b>distinct:</b> 2 session(s)") && html.contains("<b>raw:</b> 2"),
        "both counts are still printed, and both are floors:\n{html}"
    );
    // The per-copy block marks beta and not alpha.
    assert!(
        html.contains("<tr><td><b>beta</b> <span class=bad"),
        "the failing copy is marked in the per-destination block:\n{html}"
    );
    assert!(
        html.contains("<tr><td><b>alpha</b></td>"),
        "and the healthy one is not:\n{html}"
    );

    // Serving a floor is exit 3 (`did not finish reading`), not 0 and not 1.
    let clean = run(
        sb.path(),
        &[
            "ui",
            "--destination",
            "alpha,beta",
            "--no-open",
            "--idle-timeout",
            "1",
        ],
    );
    assert_eq!(
        clean.status.code(),
        Some(3),
        "an unreadable destination is exit 3, never 0: {}",
        String::from_utf8_lossy(&clean.stdout)
    );
}

/// One destination is still one destination: no badge, no column, no second
/// count, exit 0 — the ordinary dashboard, unchanged.
#[test]
fn a_single_destination_is_unchanged() {
    let sb = sandbox();
    two_destinations(sb.path());
    let (ui, narration) = Ui::start(sb.path(), &["--destination", "alpha"]);
    assert!(
        !narration.contains("merge order"),
        "one destination has no merge to explain:\n{narration}"
    );
    assert!(
        !narration.contains("raw copies"),
        "and no second reading of the count:\n{narration}"
    );
    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    assert!(
        !html.contains("×2 backup") && !html.contains("class=badge"),
        "no badge on a single-destination page:\n{html}"
    );
    assert!(
        !html.contains("<th>destination</th>"),
        "and no destination column:\n{html}"
    );
    assert!(
        !html.contains("<b>raw:</b>"),
        "and no second count:\n{html}"
    );
    let clean = run(
        sb.path(),
        &[
            "ui",
            "--destination",
            "alpha",
            "--no-open",
            "--idle-timeout",
            "1",
        ],
    );
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");
}

/// The shapes that cannot mean anything are usage errors, before anything is
/// read or served.
#[test]
fn the_shapes_that_cannot_mean_anything_are_refused() {
    let sb = sandbox();
    two_destinations(sb.path());
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["ui", "--destination", "alpha,alpha", "--no-open"],
            "twice",
        ),
        (vec!["ui", "--destination", "all,alpha", "--no-open"], "all"),
        (
            vec![
                "ui",
                "--destination",
                "alpha,beta",
                "--repo",
                "/tmp",
                "--no-open",
            ],
            "--repo",
        ),
        (
            vec![
                "ui",
                "--destination",
                "alpha,beta",
                "--key-file",
                "/tmp/k.json",
                "--no-open",
            ],
            "--key-file",
        ),
        (
            vec![
                "ui",
                "--destination",
                "alpha,beta",
                "--option",
                "k=v",
                "--no-open",
            ],
            "--option",
        ),
        (vec!["ui", "--destination", "ghost", "--no-open"], "ghost"),
    ];
    for (args, needle) in cases {
        let out = run(sb.path(), &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "`{}` must be a usage error: {out:?}",
            args.join(" ")
        );
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            stderr.contains(needle),
            "the refusal must name `{needle}`: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("http://"),
            "nothing may be served: {stdout}"
        );
    }
}

/// The index is per destination, so a merged `/search` answers from every
/// readable one — and a document both copies hold is **one** result, the same
/// rule the rows follow.
#[test]
fn the_merged_search_answers_from_every_readable_index() {
    let sb = sandbox();
    two_destinations(sb.path());
    for name in ["alpha", "beta"] {
        let build = run(sb.path(), &["index", "build", "--destination", name]);
        assert!(build.status.success(), "{name}: {build:?}");
    }
    let (ui, _) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);

    let (status, html) = ui.get("/search?q=synthetic");
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("index per destination:"),
        "a merged search must say which copy each index is:\n{html}"
    );
    assert!(
        html.contains("alpha") && html.contains("beta"),
        "both destinations' indexes must be named:\n{html}"
    );

    let (status, body) = ui.get("/api/search?q=synthetic");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["index"]["state"], "ready");
    let per_destination = v["index"]["per_destination"]
        .as_array()
        .expect("a merged server reports each copy's index");
    assert_eq!(per_destination.len(), 2);
    assert!(
        per_destination.iter().all(|part| part["state"] == "ready"),
        "{per_destination:?}"
    );
    assert_eq!(
        v["matched"], 3,
        "three distinct sessions matched once each, though two copies hold them"
    );
}
// ------------------------------------------------- W195: /export in a merge

/// `/export` under a merged view: the download of a row is the copy **that
/// row names**, byte for byte, and it is not the other destination's copy.
///
/// Both halves matter and only the second one is a test. The rule for which
/// copy a row is read from was set for `/content` and `/reader` when the merge
/// was built — the first destination named that holds the row — and `/export`
/// goes through the same `ContentSource::fetch`, so what is checked here is
/// that the new route inherits it rather than reaching for a store of its own.
/// With identical copies that property is unfalsifiable, which is why this
/// fixture seals `shared` from a different prompt in each destination and the
/// two expected bodies are asserted to differ before either is compared.
#[test]
fn a_merged_view_exports_each_row_from_the_copy_it_names() {
    let sb = sandbox();
    two_destinations_with_differing_copies(sb.path());
    let (ui, _) = Ui::start(sb.path(), &["--destination", "alpha,beta"]);

    let from_alpha = sealed_bytes(sb.path(), "alpha", SHARED);
    let from_beta = sealed_bytes(sb.path(), "beta", SHARED);
    assert!(
        from_alpha != from_beta,
        "the fixture must hold two different copies, or this test cannot tell \
         which one was served"
    );
    // …and the copy only beta holds is a third body, so `only-b` cannot be
    // answered by either of alpha's files.
    let only_b_sealed = sealed_bytes(sb.path(), "beta", ONLY_B);

    let rows = ui.rows("?limit=100");
    let index_of = |session: &str| -> String {
        let short = chat_stasher::id::short_session_id(session);
        rows.iter()
            .find(|(_, id)| *id == short)
            .map(|(i, _)| i.clone())
            .unwrap_or_else(|| panic!("no row for {short} in {rows:?}"))
    };

    // `shared` is held by both copies and alpha is named first, so alpha's
    // copy supplies the row — including the download of it.
    let (status, body) = ui.get(&format!("/export?i={}&fmt=jsonl", index_of(SHARED)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body.as_bytes(),
        from_alpha.as_slice(),
        "a merged row's download must be the copy the row names (alpha, first), \
         not the other copy and not a re-encoding"
    );
    assert_ne!(
        body.as_bytes(),
        from_beta.as_slice(),
        "the other destination's copy must not be what a download serves"
    );

    // `only-b` exists in beta alone, so the row names beta even though beta is
    // second on the command line: the rule is "the first destination that
    // holds it", not "the first destination named".
    let (status, body) = ui.get(&format!("/export?i={}&fmt=jsonl", index_of(ONLY_B)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body.as_bytes(),
        only_b_sealed.as_slice(),
        "a row only the second destination holds is read from the second \
         destination, not refused for being absent from the first"
    );

    // The one thing a merged view cannot do is spell itself as one CLI
    // command: `export --destination` takes a single value and the command has
    // no cross-destination merge. The block says so and prints no command.
    let (status, html) = ui.get("/sessions");
    assert_eq!(status, 200);
    let block = html
        .split("<section id=export-cli>")
        .nth(1)
        .expect("the block exists on a merged page too")
        .split("</section>")
        .next()
        .unwrap();
    assert!(
        !block.contains("<pre>"),
        "a merged view must not print a command that cannot run: {block}"
    );
    assert!(
        block.contains("cross-destination merge"),
        "and it must say why, not merely omit the command: {block}"
    );

    // Per-session downloads are not what was refused: the session page still
    // offers the link, and the link is the row's own copy.
    let (status, html) = ui.get(&format!("/session?i={}", index_of(SHARED)));
    assert_eq!(status, 200);
    assert!(
        html.contains("/export?i="),
        "a merged session page still offers its download: {html}"
    );
}
