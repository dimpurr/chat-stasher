//! B85-YELLOWBATCH regression tests.
//!
//! Rollback proof: before this change, the old source paths were verified by
//! reading the pre-change lines in `git show HEAD` (no checkout or write):
//! `unwrap_or_default()` rendered an unresolved root as `（）`; `root.is_dir()`
//! erased records after a concurrent removal; `metadata`/`ps` failures were
//! skipped or flattened to zero; Gemini parse failures continued to defaults;
//! and `status` used 3600 after an invalid explicit interval. Each fixture
//! below asserts the replacement answer, so reverting its corresponding hunk
//! makes that test red (or makes its changed result type fail to compile).
//!
//! All fixtures are `tempfile` paths with isolated HOME/XDG variables. They
//! contain only synthetic names, metadata and configuration; no session body,
//! real harness, archive, remote, or `.private/` path is read.

use chat_stasher::config::{self, Config};
use chat_stasher::doctor::{self, ReclaimCheck};
use chat_stasher::inbox;
use chat_stasher::models::{HarnessSource, SessionRecord};
use chat_stasher::reap;
use chat_stasher::sqlite_probe;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::SystemTime;

/// The process environment is process-global, and `cargo test` runs the tests
/// in this file as threads of one process. Two fixtures here *write* it
/// (`d4` blanks `PATH`, `d6` unsets `HOME`), and every fixture *reads* it —
/// either directly, or by spawning the binary, which snapshots the parent
/// environment at spawn time. A read that races a write gets the other
/// fixture's environment.
///
/// B97: that race was real and it was silent. `d4`'s blanked `PATH` reached a
/// concurrently spawned child; with no `PATH`, `id::machine_id()` cannot run
/// `hostname -s`, `scanner::scan` fails with "machine identity unavailable",
/// and `status` exits **3** instead of **1** — a red
/// `clean_status_output_is_byte_identical` that goes green on a rerun.
/// So *every* test in this file takes this lock, readers included. Writers
/// alone is not enough: the reader is the one that gets lied to.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const CLEAN_STATUS_BODY_SHA256: &str =
    "496303cf8515a15ccd016f5d9dfd4ef5563c1251f057cc9e75d0e4bec17e4461";

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_empty_registry(sandbox: &Path) -> PathBuf {
    let path = sandbox.join("registry.json");
    fs::write(
        &path,
        br#"{"schema_version":1,"generated":"B85","harnesses":[]}"#,
    )
    .expect("write synthetic empty registry");
    path
}

fn isolated_command(sandbox: &Path, args: &[&str], registry: &Path) -> Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    // B97: `PATH` and the machine name used to be inherited, which made every
    // spawn here depend on whatever the rest of the process had done to the
    // environment (and on which tools the host happens to ship). Both are now
    // pinned to sandbox-owned values, so the child's answer is a function of
    // this fixture alone:
    //   * `PATH` points at an empty sandbox directory — the child looks up no
    //     host tool at all, so no other fixture's `PATH` can change what it
    //     finds.
    //   * `HOSTNAME` is therefore the source `id::machine_id()` falls back to,
    //     and it is a literal. Without it the empty `PATH` would leave the
    //     machine identity unresolvable and `status` would exit 3.
    // This is belt-and-braces with `ENV_LOCK`: the lock closes the race for
    // variables nobody thought to pin, these two close it for the variables
    // that actually reached the child.
    let no_tools = sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).expect("create empty PATH directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(args)
        .env("PATH", &no_tools)
        .env("HOSTNAME", "b85-fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR");
    command
}

fn status_body(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// A registry cell with no statically resolvable root reaches the exact
/// `probe.root == None` branch that used to build an empty `PathBuf`.
#[test]
fn a6_unresolved_root_is_unknown_not_empty_parentheses() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = sandbox.path().join("registry.json");
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B85","harnesses":[{"id":"opencode","display_name":"opencode","paths":{"macos":{"template":"$B85_UNKNOWN/opencode.db","format":"sqlite","confidence":"measured-locally","source":"B85"},"linux":{"template":"$B85_UNKNOWN/opencode.db","format":"sqlite","confidence":"measured-locally","source":"B85"},"windows":{"template":"$B85_UNKNOWN/opencode.db","format":"sqlite","confidence":"measured-locally","source":"B85"}}}]}"#,
    )
    .expect("write unresolved-root registry");
    let mut command = isolated_command(sandbox.path(), &["doctor"], &registry);
    command.env("HOSTNAME", "b85-machine");
    let output = command.output().expect("run doctor");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("not installed (unknown)") && !text.contains("not installed ()"),
        "unresolved root must be rendered as unknown, not an empty path: {text}"
    );
}

/// A synthetic record is enough to prove the aggregate boundary: the path is
/// removed before aggregation, but the record itself was already obtained.
#[test]
fn b1_records_survive_a_racing_root_removal() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let root = sandbox.path().join("removed-after-scan");
    let record = SessionRecord {
        id: "synthetic.b85-race".to_string(),
        absolute_path: sandbox.path().join("synthetic.jsonl"),
        byte_size: 17,
        mtime: SystemTime::UNIX_EPOCH,
        source: HarnessSource::ClaudeCode,
        compressed: false,
        sqlite_layout: None,
    };
    let footprint = doctor::coverage_from_records("claude-code", root, [record].iter());
    assert!(
        footprint.installed && footprint.session_count == Some(1),
        "already collected records must not be erased by a later root stat: {footprint:?}"
    );
}

#[test]
fn a7_wrong_shaped_repository_is_not_reported_as_missing() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let repo = sandbox.path().join("repo-file");
    fs::write(&repo, b"synthetic repository shape").expect("write wrong-shaped repo fixture");
    let result = doctor::inspect_reclaim(&Config {
        rustic_repo: Some(repo.to_string_lossy().into_owned()),
        ..Config::default()
    });
    assert!(
        matches!(result, ReclaimCheck::OpenFailed { ref error, .. } if error.contains("not a directory")),
        "a file at repo_root is a shape error, not a measured absent repo: {result:?}"
    );
}

#[test]
fn a7b_repo_below_a_file_is_not_measured_absence() {
    // The same wrong-shape bug one level up: the *parent* of `repo_root` is a
    // regular file. Unix reports that as ENOTDIR; Windows folds it into
    // `ErrorKind::NotFound`, so a `NotFound`-means-absent read would call a
    // broken path a measured "no repository". Absence must be confirmed.
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let blocker = sandbox.path().join("repo-blocker");
    fs::write(&blocker, b"synthetic path blocker").expect("write repo parent blocker");
    let repo_root = blocker.join("repo");
    let result = doctor::inspect_reclaim(&Config {
        rustic_repo: Some(repo_root.to_string_lossy().into_owned()),
        ..Config::default()
    });
    assert!(
        matches!(result, ReclaimCheck::OpenFailed { .. }),
        "a repo_root whose parent is a file is a shape error, not measured absence: {result:?}"
    );
}

#[test]
fn a8_failed_sqlite_stat_cannot_form_a_fingerprint() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let parent = sandbox.path().join("not-a-directory");
    fs::write(&parent, b"synthetic path blocker").expect("write stat blocker");
    let db = parent.join("sessions.sqlite");
    assert!(
        sqlite_probe::sqlite_store_fingerprint(&db).is_err(),
        "a stat failure must remain unknown, not hash a zero timestamp"
    );
}

// Unix-only: "a candidate whose metadata cannot be read" is injected with a
// broken symlink. On Windows, creating a broken symlink through the standard
// API requires a dev-mode flag or admin rights (unprivileged symlink creation
// is not available at all), so the *mechanism* cannot be reproduced there.
// The property — a candidate whose metadata could not be read must not
// disappear from the count — is exercised on every platform at the store level
// by B85's `a8_failed_sqlite_stat_cannot_form_a_fingerprint` and the b82
// "unstattable is unknown, not absent" family, which reach the same
// must-not-vanish boundary through paths Windows can express.
#[cfg(unix)]
#[test]
fn c6_inbox_metadata_failure_is_an_ingest_error() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use std::os::unix::fs::symlink;

    let sandbox = tempfile::tempdir().expect("create sandbox");
    let inbox_root = sandbox.path().join("inbox");
    let stage = sandbox.path().join("stage");
    fs::create_dir_all(&inbox_root).expect("create inbox");
    symlink(
        sandbox.path().join("does-not-exist"),
        inbox_root.join("metadata-unknown.json"),
    )
    .expect("create broken synthetic inbox link");
    let report = inbox::ingest(&inbox_root, &stage, "b85-machine").expect("ingest fixture");
    assert_eq!(
        (report.total_inbox_files, report.errors.len()),
        (1, 1),
        "a candidate whose metadata could not be read must not disappear from the count"
    );
}

#[test]
fn d2_malformed_gemini_settings_are_unknown_not_default_dangerous() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let settings = sandbox.path().join(".gemini/settings.json");
    fs::create_dir_all(settings.parent().expect("settings parent")).expect("create Gemini dir");
    fs::write(&settings, b"{ malformed synthetic settings").expect("write malformed settings");
    let retention = doctor::inspect_gemini_settings(sandbox.path());
    assert!(
        retention.summarize().contains("unknown") && !retention.is_dangerous(),
        "a present but unreadable policy must not be judged using 30d defaults: {retention:?}"
    );
}

#[test]
fn d4_unavailable_ps_returns_unknown_reap_count() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let old_path = std::env::var_os("PATH");
    let sandbox = tempfile::tempdir().expect("create sandbox");
    std::env::set_var("PATH", sandbox.path());
    let result = reap::reap_masters_for_host("b85.synthetic.invalid");
    match old_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    assert!(
        result.is_err(),
        "when ps cannot run, reaping must not return the user-visible number 0"
    );
}

#[test]
fn d6_missing_home_and_invalid_interval_are_not_silently_replaced() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let old_home = std::env::var_os("HOME");
    let old_profile = std::env::var_os("USERPROFILE");
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let profile = sandbox.path().join("profile");
    std::env::remove_var("HOME");
    std::env::set_var("USERPROFILE", &profile);
    let resolved = config::home_dir();
    match old_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match old_profile {
        Some(value) => std::env::set_var("USERPROFILE", value),
        None => std::env::remove_var("USERPROFILE"),
    }

    let registry = write_empty_registry(sandbox.path());
    let config_dir = sandbox.path().join("xdg-config/chat-stasher");
    fs::create_dir_all(&config_dir).expect("create isolated config");
    fs::write(
        config_dir.join("config.toml"),
        b"backup_interval_secs = 0\n",
    )
    .expect("write invalid interval config");
    let output = isolated_command(sandbox.path(), &["status"], &registry)
        .output()
        .expect("run status with invalid interval");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        resolved == profile
            && text.contains("backup_interval_secs is invalid")
            && !text.contains("threshold 3600"),
        "HOME must not become cwd and invalid explicit interval must not become silent 3600: {text}"
    );
}

#[test]
fn clean_status_output_is_byte_identical() {
    // B97: see `ENV_LOCK` — readers serialize with writers too.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let root = sandbox.path().join("source");
    fs::create_dir_all(&root).expect("create clean synthetic source");
    fs::write(root.join("session.jsonl"), b"synthetic metadata fixture\n")
        .expect("write synthetic session fixture");
    let registry = sandbox.path().join("registry.json");
    let root_json = serde_json::json!({
        "template": root,
        "format": "jsonl",
        "session_pattern": "*.jsonl",
        "confidence": "measured-locally",
        "source": "B85 clean fixture"
    });
    let registry_json = serde_json::json!({
        "schema_version": 1,
        "generated": "B85",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "Claude Code",
            "paths": {"macos": root_json.clone(), "linux": root_json.clone(), "windows": root_json}
        }]
    });
    fs::write(
        &registry,
        serde_json::to_vec(&registry_json).expect("serialize clean registry"),
    )
    .expect("write clean registry");
    let output = isolated_command(sandbox.path(), &["status"], &registry)
        .output()
        .expect("run clean status");
    let body = status_body(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the clean fixture keeps the established status exit code"
    );
    assert!(
        !body.contains("scan partial") && !body.contains("unreadable"),
        "false-positive partial-scan guard fired: {body}"
    );
    assert_eq!(
        sha256_hex(body.as_bytes()),
        CLEAN_STATUS_BODY_SHA256,
        "clean status output changed: {body}"
    );
}
