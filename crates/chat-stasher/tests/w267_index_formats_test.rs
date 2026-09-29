//! W267 — what the local full-text index reads out of an archive, per harness
//! format, and what it says about a shard it cannot read.
//!
//! The field test (W255 C2) found 2,048 of 7,187 indexed sessions with an
//! **empty body** while every surface counted them as "indexed": the extractor
//! understood three of the twelve local harness formats. This test drives the
//! real binary over a real repository holding one shard per shape, and reads
//! the counts back off the CLI, because the property is about *counting* — a
//! session nobody could read must not be reported as covered.
//!
//! Everything here is synthetic: the shards are hand-written fixtures in the
//! shapes the capture side seals (`sqlite_probe.rs`) and the reader pins
//! (`normalize/<harness>.rs`). No machine's archive is read, and no
//! conversation text is asserted on beyond the words this file writes itself.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

// --------------------------------------------------------------- sandbox

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
}

/// Run the CLI with a sandboxed HOME/XDG and an empty registry, so nothing
/// reads or writes the real machine's config, registry or stage.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W267 synthetic","harnesses":[]}"#,
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

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "the command failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The JSON a `search` answered with, whatever its exit code — a search whose
/// selection holds an unreadable session still answers, and exits `3`
/// ("did not finish reading"), which is what the caller reads the answer with.
fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "the command did not answer JSON ({error}): {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

// --------------------------------------------------------------- fixtures

const MACHINE: &str = "mbp-w267";

/// One session per shape the index has an arm for.
const CLAUDE_CODE: &str = "claude-code.mbp-w267.019bf00d-97b6-7eb2-9bf8-eacbacc0a001";
const OPENCODE: &str = "opencode.mbp-w267.ses_019bf00d97b67eb2";
const GROK: &str = "grok.mbp-w267.019bf00d-97b6-7eb2-9bf8-eacbacc0a003";
const ZED: &str = "zed.mbp-w267.019bf00d-97b6-7eb2-9bf8-eacbacc0a004";

/// A JSONL record in `claude-code`'s own shape.
const CLAUDE_CODE_SHARD: &str = concat!(
    r#"{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","#,
    r#""message":{"role":"user","content":"a synthetic question about a hedgehog"},"#,
    r#""uuid":"u1","timestamp":"2025-01-15T12:00:00Z","cwd":"/x","version":"1.0.31"}"#,
);

/// An `opencode` export: one whole session as one line, in the shape
/// `sqlite_probe.rs` seals. Read by the reader's own opencode extractor.
const OPENCODE_SHARD: &str = concat!(
    r#"{"schema":"chat-stasher.opencode.session.v1","session":{"id":"ses_019bf00d97b67eb2","#,
    r#""time_created":1770000000000,"time_updated":1770000000001},"messages":[{"#,
    r#""id":"m1","session_id":"ses_019bf00d97b67eb2","time_created":1770000000000,"#,
    r#""time_updated":1770000000000,"data":{"role":"user"},"parts":[{"#,
    r#""id":"p1","message_id":"m1","session_id":"ses_019bf00d97b67eb2","#,
    r#""time_created":1770000000000,"time_updated":1770000000000,"#,
    r#""data":{"type":"text","text":"a synthetic question about a wombat"}}]}],"orphan_parts":[]}"#,
);

/// A `grok` CLI row: the exported `session_docs` row, whose conversation body
/// lives in a per-session directory this archive does not hold. There is no
/// message text in it, and that is a format this build cannot read.
const GROK_SHARD: &str = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"019bf00d-97b6-7eb2-9bf8-eacbacc0a003","updated_at":1784924765}}"#;

fn write_shard(stage: &Path, session: &str, bytes: &[u8]) {
    let dir = stage
        .join("sessions")
        .join(MACHINE)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), bytes).unwrap();
}

/// A destination repository holding the fixture shards, plus its masterkey.
///
/// The stage is sealed with `push`, so what the index reads is what the
/// archive holds — not the stage — which is the path the field test measured.
fn make_repo(sandbox: &Path) -> (String, String) {
    let stage = sandbox.join("stage");
    write_shard(&stage, CLAUDE_CODE, CLAUDE_CODE_SHARD.as_bytes());
    write_shard(&stage, OPENCODE, OPENCODE_SHARD.as_bytes());
    write_shard(&stage, GROK, GROK_SHARD.as_bytes());
    // A SQLite database archived whole: bytes, not a text format.
    write_shard(
        &stage,
        ZED,
        b"SQLite format 3\0\x00\x01\x02\x03\xff\xfe\x00",
    );
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let index = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
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
            MACHINE,
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "{push:?}");
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.alpha]\nrepo = '{}'\nkey_file = '{}'\n",
            repo.to_string_lossy(),
            key.to_string_lossy()
        ),
    )
    .unwrap();
    (
        repo.to_string_lossy().into_owned(),
        key.to_string_lossy().into_owned(),
    )
}

// ------------------------------------------------------------------ tests

/// A shard the index cannot read is neither searchable nor counted as
/// searched: it is held, and reported as a format this build cannot read.
///
/// The three numbers below are the whole issue. Before the per-format layer,
/// the opencode and grok sessions both indexed with an empty body and the
/// coverage line said `index_covered=4` of 4 — a claim that a query had been
/// run over text nobody had read.
#[test]
fn a_shard_the_index_cannot_read_is_not_counted_as_covered() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());

    let build = run(
        sandbox.path(),
        &["index", "build", "--destination", "alpha"],
    );
    let stdout = stdout_of(&build);
    assert!(
        stdout.contains("documents=4"),
        "every archived session is held by the index: {stdout}"
    );

    let found = json_of(&run(
        sandbox.path(),
        &[
            "search",
            "--destination",
            "alpha",
            "--text",
            "wombat",
            "--json",
        ],
    ));
    assert_eq!(
        found["matched"], 1,
        "the opencode session's own text is searchable: {found}"
    );

    let searched = run(
        sandbox.path(),
        &[
            "search",
            "--destination",
            "alpha",
            "--text",
            "hedgehog",
            "--json",
        ],
    );
    // Two of the four sessions in view were never read, so the search did not
    // finish reading and says so with `3` — the code that keeps "we did not
    // look" from being read as "we looked and it is not there".
    assert_eq!(
        searched.status.code(),
        Some(3),
        "a search over an unreadable session did not finish reading: {}",
        String::from_utf8_lossy(&searched.stderr)
    );
    let all = json_of(&searched);
    assert_eq!(all["selected"], 4, "all four sessions are in view");
    assert_eq!(
        all["index_covered"], 2,
        "the claude-code and opencode sessions are the ones with text"
    );
    assert_eq!(
        all["index_not_indexable"], 2,
        "the grok row and the SQLite database are held and unreadable"
    );
    assert_eq!(all["index_missing"], 0, "nothing in view is missing");
    assert_eq!(
        all["index_not_indexable_formats"], "sqlite 2",
        "the format is what a reader can act on: {all}"
    );
}

/// `index check` reports the unreadable documents beside the ones it holds,
/// so an index of 4 documents that can answer for 2 cannot be read as one that
/// can answer for 4.
#[test]
fn index_check_names_what_it_holds_and_what_it_cannot_read() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());
    let build = run(
        sandbox.path(),
        &["index", "build", "--destination", "alpha"],
    );
    assert!(build.status.success(), "{build:?}");

    let stdout = stdout_of(&run(
        sandbox.path(),
        &["index", "check", "--destination", "alpha"],
    ));
    // `documents` is what the index holds; `state=partial` is the build's
    // outcome, and two sources it could not read is what makes it partial
    // rather than valid. Both are reported: a document count alone would read
    // as an index that answers for all four.
    assert!(
        stdout.contains("[index] state=partial documents=4"),
        "{stdout}"
    );
    assert!(
        stdout.contains("last_build_not_indexable=2 last_build_indexed=2"),
        "the build's own report of what it read and what it could not: {stdout}"
    );
    assert!(
        stdout.contains("[index] not_indexable=2 (sqlite 2)"),
        "the count and the format must both be reported: {stdout}"
    );
}

/// A session id finds its session even though the id is not conversation text,
/// and a **prefix** of it does too — the shape a reader reaches for when they
/// know which session they want (W255 C5).
#[test]
fn a_session_id_finds_its_session() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());
    let build = run(
        sandbox.path(),
        &["index", "build", "--destination", "alpha"],
    );
    assert!(build.status.success(), "{build:?}");

    for query in ["019bf00d-97b6-7eb2-9bf8-eacbacc0a001", "019bf00d-97b6"] {
        let found = json_of(&run(
            sandbox.path(),
            &[
                "search",
                "--destination",
                "alpha",
                "--text",
                query,
                "--json",
            ],
        ));
        assert!(
            found["matched"].as_u64().unwrap_or(0) >= 1,
            "`{query}` is an id prefix and must find the session: {found}"
        );
    }
}
