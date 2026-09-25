//! W156 — `chat-stasher ui` picks the destination to open when the config makes
//! the choice unambiguous, and keeps refusing to guess everywhere else.
//!
//! Everything here runs the real binary against real rustic repositories built
//! from synthetic stages. No machine's archive is touched.
//!
//! Properties pinned (per the W156 brief):
//!
//! * With exactly one `[destinations.<name>]` declared, `ui` opens it with no
//!   `--repo`/`--destination`, and the narration says which default it took.
//! * With several declared, `[native_host] destination` — the one knob the
//!   config already has for "which destination the dashboard opens" — is the
//!   documented default; without it `ui` lists them and asks (exit 2); naming
//!   an undeclared one is a config bug it refuses (exit 2), never a silent
//!   fallback.
//! * With none declared and no `--repo`, `ui` explains how to declare one
//!   (exit 2) — never "serving an empty archive".
//! * The explicit flag still wins over every default.
//! * `search` and friends keep requiring the copy to be named (ADR-013): the
//!   default belongs to `ui` because a dashboard is a look, not a retrieval.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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

/// Write a config (TOML body verbatim) where `Config::load` will find it.
fn write_config(sandbox: &Path, body: &str) {
    let dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

// --------------------------------------------------------------- fixture

fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

const T: &str = "2025-01-15T12:00:00Z";

/// A minimal readable repository one destination can point at: one
/// claude-code session, indexed and pushed.
fn one_repo(sandbox: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let stage = sandbox.join("stage-solo");
    let session = "claude-code.mbp-solo.019bf00d-97b6-7eb2-9bf8-eaccaaa0cc01";
    let prompt = "a one-destination fixture prompt";
    let line = format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"{prompt}"}},"uuid":"u1","timestamp":"{T}","cwd":"/x","version":"1.0.31"}}"#
    );
    write_shard(&stage, "mbp-solo", session, &[line]);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let index = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-solo",
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
            "mbp-solo",
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "{push:?}");
    (repo, key)
}

/// `[destinations.<name>]` rows pointing at a repository and its key.
fn dest_config(name: &str, repo: &Path, key: &Path) -> String {
    format!(
        "[destinations.{name}]\nrepo = '{repo}'\nkey_file = '{key}'\n",
        repo = repo.display(),
        key = key.display(),
    )
}

// ------------------------------------------------------------------- tests

/// With exactly one destination declared, `ui` opens it with no flags — and
/// says which default it took, so the choice never reads as a guess.
#[test]
fn ui_opens_the_only_declared_destination() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    write_config(sb.path(), &dest_config("solo", &repo, &key));
    let out = run(sb.path(), &["ui", "--no-open", "--idle-timeout", "1"]);
    assert!(
        out.status.success(),
        "ui must open the one destination: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("[ui] destination  : solo"),
        "the narration must name the default it chose: {stdout}"
    );
    assert!(
        stdout.contains("[ui] default"),
        "a default that is not narrated reads the same as a guess: {stdout}"
    );
    assert!(stdout.contains("http://"), "a dashboard must have served");
}

/// With several destinations, `ui` lists them and asks — exit 2, a usage
/// question — when no default is recorded.
#[test]
fn ui_lists_and_asks_when_several_destinations_are_declared() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    // The second destination deliberately points at nothing: the message is
    // about the *choice*, not about either repository being readable.
    write_config(
        sb.path(),
        &format!(
            "{}[destinations.unbuilt]\nrepo = '{}'\n",
            dest_config("alpha", &repo, &key),
            sb.path().join("no-such-repo").display(),
        ),
    );
    let out = run(sb.path(), &["ui", "--no-open", "--idle-timeout", "1"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage question, not a guessed archive: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("2 destination(s)"),
        "the choice must be counted: {stderr}"
    );
    assert!(
        stderr.contains("alpha") && stderr.contains("unbuilt"),
        "both names must be listed: {stderr}"
    );
    assert!(
        stderr.contains("--destination"),
        "the ask must name the flag that answers it: {stderr}"
    );
    assert!(
        stderr.contains("native_host"),
        "the ask must name the knob that records a default: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("http://"),
        "nothing may be served: {stdout}"
    );
}

/// When several are declared and the config's `[native_host] destination` —
/// the documented "which destination the dashboard opens" knob — names a
/// declared one, `ui` opens that one and says where the default came from.
#[test]
fn ui_uses_the_native_host_default_when_several_are_declared() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    write_config(
        sb.path(),
        &format!(
            "{tail}[native_host]\ndestination = \"alpha\"\n[destinations.unbuilt]\nrepo = '{}'\n",
            sb.path().join("no-such-repo").display(),
            tail = dest_config("alpha", &repo, &key),
        ),
    );
    let out = run(sb.path(), &["ui", "--no-open", "--idle-timeout", "1"]);
    assert!(
        out.status.success(),
        "the declared default must open: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("[ui] destination  : alpha"),
        "the narration must name the default it took: {stdout}"
    );
    assert!(
        stdout.contains("native_host"),
        "it must say where the default came from: {stdout}"
    );
}

/// A `[native_host] destination` naming an undeclared destination is a config
/// bug: `ui` refuses with the mismatch named, exit 2 — never a silent fallback
/// to some other destination.
#[test]
fn ui_refuses_a_native_host_default_that_is_not_declared() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    write_config(
        sb.path(),
        &format!(
            "{tail}[native_host]\ndestination = \"ghost\"\n",
            tail = dest_config("solo", &repo, &key),
        ),
    );
    let out = run(sb.path(), &["ui", "--no-open", "--idle-timeout", "1"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a broken default is a usage error: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("ghost"),
        "the offending name must be in the message: {stderr}"
    );
    assert!(
        stderr.contains("not"),
        "the mismatch must be stated, not implied: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("http://"),
        "nothing may be served: {stdout}"
    );
}

/// With no destination declared and no `--repo`, there is nothing to open:
/// `ui` explains how to point it at one, exit 2 — never "serving an empty
/// archive".
#[test]
fn ui_explains_when_there_is_nothing_to_open() {
    let sb = sandbox();
    let (repo, _key) = one_repo(sb.path());
    // Built but deliberately not declared anywhere.
    drop(repo);
    let out = run(sb.path(), &["ui", "--no-open", "--idle-timeout", "1"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage question, not an empty dashboard: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("declares no destination"),
        "the state must be named: {stderr}"
    );
    assert!(
        stderr.contains("--repo"),
        "the way out must be named: {stderr}"
    );
    assert!(
        stderr.contains("[destinations."),
        "the way to record one must be named: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("http://"),
        "nothing may be served: {stdout}"
    );
}

/// The explicit flag still wins over every default — the default only exists
/// where no choice was expressed.
#[test]
fn an_explicit_destination_beats_the_default() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    write_config(
        sb.path(),
        &format!(
            "{tail}[destinations.unbuilt]\nrepo = '{}'\n[native_host]\ndestination = \"unbuilt\"\n",
            sb.path().join("no-such-repo").display(),
            tail = dest_config("solo", &repo, &key),
        ),
    );
    // `--repo` wins over a config default that would otherwise answer
    // "unbuilt".
    let out = run(
        sb.path(),
        &[
            "ui",
            "--no-open",
            "--idle-timeout",
            "1",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && !stdout.contains("[ui] destination  : unbuilt"),
        "--repo must override every default: {out:?}\n{stdout}"
    );
    // And `--destination` wins too: naming the good one in a 2-destination
    // config opens it even though the [native_host] default names the bad one.
    let out = run(
        sb.path(),
        &[
            "ui",
            "--no-open",
            "--idle-timeout",
            "1",
            "--destination",
            "solo",
        ],
    );
    assert!(
        out.status.success(),
        "--destination must override the default: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("[ui] destination  : solo"), "{stdout}");
}

/// ADR-013 keeps its teeth outside `ui`: with several destinations declared,
/// retrieval commands still require naming the copy — `ui`'s default does not
/// leak into `search`.
#[test]
fn retrieval_commands_keep_refusing_a_default_destination() {
    let sb = sandbox();
    let (repo, key) = one_repo(sb.path());
    write_config(
        sb.path(),
        &format!(
            "{}[destinations.unbuilt]\nrepo = '{}'\n",
            dest_config("alpha", &repo, &key),
            sb.path().join("no-such-repo").display(),
        ),
    );
    let out = run(sb.path(), &["search", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "search must keep asking which archive, even when ui would not: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("--destination"),
        "the ask must name the flag: {stderr}"
    );
    assert!(
        stderr.contains("no default"),
        "the ADR-13 wording must stay: {stderr}"
    );
}
