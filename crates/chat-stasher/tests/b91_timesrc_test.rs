//! B91 — time source and configuration provenance regression tests.
//!
//! These tests are intentionally written against the old implementation first:
//! the time-label and config-provenance assertions must be red before the fix.
//! Every CLI run uses only paths below a `tempfile` directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn isolated_env(sandbox: &Path, args: &[&str], registry: &Path) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
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
        .env_remove("CURSOR_USER_DIR")
        .output()
        .unwrap()
}

fn registry_for_empty_fixture(sandbox: &Path) -> PathBuf {
    let root = sandbox.join("empty-source");
    fs::create_dir_all(&root).unwrap();
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        format!(
            r#"{{"schema_version":1,"generated":"B91","harnesses":[{{"id":"claude-code","display_name":"Claude Code","paths":{{"macos":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"measured-locally","source":"B91"}},"linux":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"measured-locally","source":"B91"}},"windows":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"measured-locally","source":"B91"}}}}}}]}}"#,
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&root).unwrap(),
        ),
    )
    .unwrap();
    registry
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A's old behaviour counterexample, now inverted.
///
/// It used to require that `search --help` say "archive time": the public query
/// was described as a session-activity filter while the metadata it actually
/// had was the snapshot time. Both halves of that sentence were wrong — the
/// help said activity, the filter did snapshot time — and ADR-027 removed the
/// gap from the other end: the filter now really does compare activity time.
///
/// So the assertion flips. Help must name the *conversation*, and must not
/// quietly still be advertising the snapshot time as what `--since` means.
#[test]
fn search_time_window_help_names_conversation_time_not_archive_time() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let out = isolated_env(sandbox.path(), &["search", "--help"], &registry);
    let text = combined(&out);
    assert!(
        text.contains("conversation"),
        "time-window help must say whose time it compares:\n{text}"
    );
    assert!(
        !text.contains("archive time") && !text.contains("rustic snapshot time"),
        "help must not still describe the window as archive/snapshot time:\n{text}"
    );
    for flag in ["--day", "--since", "--until", "--harness", "--json"] {
        assert!(
            text.contains(flag),
            "`{flag}` must be discoverable from help:\n{text}"
        );
    }
}

/// Date flags and unix-seconds flags are two spellings of one thing, so mixing
/// them is ambiguous and must be refused as a usage error — before anything is
/// opened, so the sandbox needs no repository at all.
#[test]
fn mixing_date_and_unix_time_flags_is_a_usage_error() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    for args in [
        vec!["search", "--since", "2026-01-15", "--since-unix", "100"],
        vec!["search", "--until", "2026-01-15", "--until-unix", "100"],
        vec!["search", "--day", "2026-01-15", "--since-unix", "100"],
        vec!["search", "--day", "2026-01-15", "--until", "2026-01-16"],
    ] {
        let out = isolated_env(sandbox.path(), &args, &registry);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} must be a usage error, got {:?}\n{}",
            out.status.code(),
            combined(&out)
        );
    }
}

/// A date that is not a date is refused, not guessed at. Nothing was read and
/// nothing matched, so this is a usage error (2), never "0 results" (1).
#[test]
fn malformed_dates_are_refused_before_anything_is_read() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    for bad in ["2026-1-5", "20260105", "2026-02-30", "yesterday"] {
        let out = isolated_env(
            sandbox.path(),
            &["search", "--destination", "nope", "--day", bad],
            &registry,
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "`--day {bad}` must be a usage error, got {:?}\n{}",
            out.status.code(),
            combined(&out)
        );
        assert!(
            combined(&out).contains("YYYY-MM-DD"),
            "the error must say the accepted shape:\n{}",
            combined(&out)
        );
    }
}

/// The deprecated flags still work and still mean conversation time, but they
/// say so on stderr. Checked without a repository: the notice has to be printed
/// before any read is attempted, otherwise a later failure would swallow it.
#[test]
fn deprecated_unix_flags_warn_on_stderr() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let out = isolated_env(
        sandbox.path(),
        &["search", "--destination", "nope", "--since-unix", "100"],
        &registry,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated"),
        "the notice must reach stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--day"),
        "the notice must name the replacement:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("deprecated"),
        "a machine reading stdout must not have to filter prose out of it"
    );
}

/// B's old behaviour counterexample: malformed config warns on stderr but
/// status/doctor expose no machine-readable fact that defaults were used.
#[test]
fn config_parse_fallback_is_visible_to_status_and_doctor() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let config = sandbox.path().join("xdg-config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "this is not valid TOML = [\n").unwrap();

    for command in ["status", "doctor"] {
        let out = isolated_env(sandbox.path(), &[command], &registry);
        let text = combined(&out);
        assert!(
            text.contains("config_source=defaults_after_parse_error"),
            "{command} must expose that defaults came from a config parse failure;\n{text}"
        );
    }
}

/// Health guard: a fully inspectable empty fixture must not acquire a cloud of
/// new `unknown` labels merely because byte provenance became explicit.
#[test]
fn healthy_fixture_status_and_doctor_unknown_counts_are_stable() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let status = isolated_env(sandbox.path(), &["status"], &registry);
    let doctor = isolated_env(sandbox.path(), &["doctor"], &registry);
    let status_text = combined(&status);
    let doctor_text = combined(&doctor);
    let status_unknown = status_text
        .matches("The earliest session time is unknown")
        .count();
    let doctor_unknown = doctor_text
        .matches("The earliest session time is unknown")
        .count();
    println!(
        "B91 healthy fixture: status_unknown={status_unknown} doctor_unknown={doctor_unknown}"
    );
    println!("B91 status:\n{status_text}");
    println!("B91 doctor:\n{doctor_text}");
    assert_eq!(
        status_unknown, 0,
        "healthy status grew an unexplained unknown-earliest-session line"
    );
    assert_eq!(
        doctor_unknown, 1,
        "healthy doctor must retain exactly its existing unknown earliest-session risk"
    );
    assert!(doctor_text.contains("The earliest session time is unknown"));
}
