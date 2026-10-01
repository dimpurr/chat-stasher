//! W274 — a query the index cannot evaluate is not a zero-result.
//!
//! `chat-stasher search --text ke` is two characters, below the three the
//! trigram tokenizer needs (`fts::MIN_QUERY_CHARS`). The index can return no
//! candidate at all for it, so the query was never evaluated — and yet the
//! command printed `matched=0` beside a suggestion and exited `1`. Exit `1` in
//! this CLI means "read it all and none matched", a proven negative; the run
//! had proved nothing. That is the first invariant of this repository
//! (`CLAUDE.md`: an unknown must never be recorded as empty) failing at the
//! one surface that a script reads by integer alone.
//!
//! The dashboard has said the right thing all along — "This query cannot be
//! evaluated… **Nothing was searched** — this is not a zero-result"
//! (`ui/search.rs`, 29-UI-DESIGN §6.2.3), on the page and as
//! `query_state: "too_short"` in `/api/search`. This file pins the CLI to the
//! same three states, and pins the two neighbours so the fix cannot overshoot:
//!
//!   * `fts` mode with a short query exits `3` ("not answerable"), and its JSON
//!     says `query_state: "too_short"` with `matched: null` — not `0`, which is
//!     a measurement;
//!   * `--scan` evaluates the *same* short query, because that mode matches the
//!     stored text itself, so `0` there is a real measurement and exit `1`
//!     stays correct (it is the escape hatch `docs/troubleshooting.md` names);
//!   * an answerable query that genuinely matches nothing still exits `1`.
//!
//! The fixture is synthetic: one hand-written `claude-code` JSONL shard, in the
//! shape the capture side seals and the reader pins. Nothing here reads a real
//! archive, and no conversation text is asserted on beyond the words this file
//! writes itself.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

// --------------------------------------------------------------- sandbox

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
}

/// Run the CLI with a sandboxed HOME/XDG and an empty registry, so nothing
/// reads or writes the real machine's config, registry, stage or cache.
///
/// `%LOCALAPPDATA%` is set beside `HOME` for the reason `w267` spells out:
/// on Windows the cache root is not a child of `$HOME` at all, so a child given
/// only `HOME` would read and write the real user's index while every path it
/// prints looks sandboxed.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let cache = sandbox.join("cache");
    let registry = sandbox.join("registry.json");
    for dir in [&home, &cache] {
        fs::create_dir_all(dir).unwrap();
    }
    if !registry.exists() {
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W274 synthetic","harnesses":[]}"#,
        )
        .unwrap();
    }
    bin()
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", &cache)
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The JSON a `search` answered with, whatever its exit code: a search that did
/// not finish reading still answers, and the answer is what says why.
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

const MACHINE: &str = "mbp-w274";
const SESSION: &str = "claude-code.mbp-w274.019bf00d-97b6-7eb2-9bf8-eacbacc0a274";

/// One JSONL record in `claude-code`'s own shape, holding `kepler`.
///
/// The word is the point of the fixture: a two-character query has something
/// real to find *if it is evaluated at all*, which is exactly what `--scan`
/// does and the trigram index cannot. A body with no `ke` in it would let a
/// broken `--scan` pass this file's second test for the wrong reason.
const SHARD: &str = concat!(
    r#"{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","#,
    r#""message":{"role":"user","content":"a synthetic question about a hedgehog called kepler"},"#,
    r#""uuid":"u1","timestamp":"2025-01-15T12:00:00Z","cwd":"/x","version":"1.0.31"}"#,
);

/// A destination holding one archived session, plus the config that names it.
fn make_repo(sandbox: &Path) {
    let stage = sandbox.join("stage");
    let dir = stage
        .join("sessions")
        .join(MACHINE)
        .join(SESSION)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), SHARD).unwrap();

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    for args in [
        vec![
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
        vec![
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
    ] {
        let output = run(sandbox, &args);
        assert!(
            output.status.success(),
            "fixture setup `{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
    let build = run(sandbox, &["index", "build", "--destination", "alpha"]);
    assert!(build.status.success(), "{build:?}");
}

// ------------------------------------------------------------------ tests

/// The query the trigram index cannot evaluate is exit `3`, and no surface of
/// the run records it as a count of zero.
///
/// `matched=0` was the old report. `0` is a measurement of the archive; here
/// nothing was measured, so the human line says `unknown` and the JSON says
/// `null` with the state named beside it.
#[test]
fn a_query_the_index_cannot_evaluate_is_not_a_zero_result() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());

    let short = run(
        sandbox.path(),
        &["search", "--destination", "alpha", "--text", "ke", "--json"],
    );
    assert_eq!(
        short.status.code(),
        Some(3),
        "a query the index cannot evaluate was never answered: {}",
        String::from_utf8_lossy(&short.stderr)
    );
    let answer = json_of(&short);
    assert_eq!(
        answer["query_state"],
        serde_json::json!("too_short"),
        "the state has to be named for a consumer that never reads a sentence: {answer}"
    );
    assert!(
        answer["matched"].is_null(),
        "a query that was never run has no match count; `0` would be a measurement: {answer}"
    );
    assert_eq!(
        answer["query_length"],
        serde_json::json!(2),
        "the length that was refused, beside the rule it broke: {answer}"
    );
    assert_eq!(
        answer["query_minimum"],
        serde_json::json!(3),
        "the tokenizer's minimum is what a caller needs to say which length would \
         have been answered: {answer}"
    );

    // The human report carries the same distinction, not just the exit code: a
    // reader who sees `matched=0` and nothing else has been told a zero.
    let human = run(
        sandbox.path(),
        &["search", "--destination", "alpha", "--text", "ke"],
    );
    assert_eq!(
        human.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let report = stdout_of(&human);
    assert!(
        report.contains("[search] matched=unknown"),
        "the report must say the count is unknown: {report}"
    );
    assert!(
        !report.contains("[search] matched=0"),
        "a query the index cannot evaluate must never be printed as a measurement of zero: {report}"
    );
}

/// The same short query *is* answered by `--scan`, which matches the stored
/// text instead of the trigram index — so its `0` would be a real measurement,
/// and a fix that refused every short query would be wrong.
///
/// This is the escape hatch the documentation has always named for a reader who
/// wants to search fewer than three characters, and it finds `kepler`.
#[test]
fn the_same_short_query_is_answered_when_the_text_itself_is_read() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());

    let scanned = run(
        sandbox.path(),
        &[
            "search",
            "--destination",
            "alpha",
            "--text",
            "ke",
            "--scan",
            "--json",
        ],
    );
    assert_eq!(
        scanned.status.code(),
        Some(0),
        "--scan evaluates the query, and the fixture's body holds it: {}",
        String::from_utf8_lossy(&scanned.stderr)
    );
    let answer = json_of(&scanned);
    assert_eq!(
        answer["query_state"],
        serde_json::json!("answered"),
        "a query that was evaluated, however few characters it has, is answered: {answer}"
    );
    assert_eq!(
        answer["matched"], 1,
        "`kepler` holds the query, so an evaluated `ke` matches: {answer}"
    );
    assert_eq!(
        answer["query_minimum"],
        serde_json::json!(3),
        "the rule is reported whether or not a query broke it: {answer}"
    );
}

/// An answerable query that matches nothing still exits `1`. The new state must
/// not swallow the negative this CLI spends `1` on.
#[test]
fn an_answerable_query_that_matches_nothing_is_still_a_negative() {
    let sandbox = tempfile::TempDir::new().unwrap();
    make_repo(sandbox.path());

    let none = run(
        sandbox.path(),
        &[
            "search",
            "--destination",
            "alpha",
            "--text",
            "zebra",
            "--json",
        ],
    );
    assert_eq!(
        none.status.code(),
        Some(1),
        "read in full, and the query is not there: {}",
        String::from_utf8_lossy(&none.stderr)
    );
    let answer = json_of(&none);
    assert_eq!(
        answer["query_state"],
        serde_json::json!("answered"),
        "{answer}"
    );
    assert_eq!(answer["matched"], 0, "{answer}");
    assert_eq!(answer["query_minimum"], serde_json::json!(3), "{answer}");
}
