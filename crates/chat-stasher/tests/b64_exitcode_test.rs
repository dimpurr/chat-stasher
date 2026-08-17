//! B64 regression: `collect` already printed "not archivable", but still exited
//! 0 — the report said the run was incomplete while the exit code said it was
//! clean. A caller that only reads the exit code was told a lie.
//!
//! The fixture is synthetic: a registry file describing one harness plus a file
//! that is deliberately *not* a SQLite database. No real harness store is read,
//! and the assertions inspect exit codes and metadata-only diagnostic lines
//! only — never session content.
//!
//! Exit-code contract asserted here (`Commands::Collect` doc comment):
//!   0 — everything recognised was archivable and was collected
//!   3 — PARTIAL: the pass ran, but a harness this build cannot archive was
//!       recognised, so the stage is real but incomplete
//!   1 — the pass failed (unchanged; not exercised here)

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// One registry harness, `opencode`, whose store is a single SQLite file taken
/// straight from `$OPENCODE_DB` (`scanner::root_from_env_override`).
fn write_registry(path: &Path) {
    let cell = r#"{
        "template": "$OPENCODE_DB",
        "env_override": "OPENCODE_DB",
        "format": "sqlite",
        "confidence": "源码确认",
        "source": "B64 synthetic fixture"
    }"#;
    fs::write(
        path,
        format!(
            r#"{{
                "schema_version": 1,
                "generated": "B64 synthetic fixture",
                "harnesses": [{{
                    "id": "opencode",
                    "display_name": "synthetic opencode",
                    "paths": {{
                        "macos": {cell},
                        "linux": {cell},
                        "windows": {cell}
                    }}
                }}]
            }}"#
        ),
    )
    .unwrap();
}

fn run_collect(sandbox: &Path, opencode_db: &Path) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = sandbox.join("registry.json");
    write_registry(&registry);

    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "collect",
            "--stage",
            sandbox.join("stage").to_str().unwrap(),
            "--machine",
            "b64-fixture",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env("OPENCODE_DB", opencode_db)
        .env_remove("CODEX_HOME")
        .output()
        .unwrap()
}

#[test]
fn collect_that_printed_not_archivable_does_not_exit_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    // Exists, so the harness probes as an installed single-file store; not a
    // SQLite database, so enumeration fails and no SessionRecord is produced.
    // That is exactly `ScanReport::archive_gaps`' "recognised, not archivable".
    let db = sandbox.path().join("not-a-sqlite.db");
    fs::write(&db, b"this is not a sqlite database\n").unwrap();

    let output = run_collect(sandbox.path(), &db);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The exit code is the point of this test.
    assert_eq!(
        output.status.code(),
        Some(3),
        "an incomplete collect must exit 3 (PARTIAL), not 0 and not 1;\nstdout={stdout}\nstderr={stderr}"
    );

    // ...and it must be an *addition* to the existing honesty, not a
    // replacement for it. Changing the printed report instead of the exit code
    // would be the same bug wearing a different hat.
    assert!(
        stdout.contains("[collect] not archivable  : 1 harness(es)"),
        "the 'did not finish' line must still be printed; stdout={stdout}"
    );
    assert!(
        stdout.contains("harness=opencode")
            && stdout.contains("session_records=0")
            && stdout.contains("source_not_collected=true"),
        "the per-harness gap detail must still be printed; stdout={stdout}"
    );
    assert!(
        stderr.contains("PARTIAL exit_code=3"),
        "the exit code must be explained in words too; stderr={stderr}"
    );
}

#[test]
fn collect_without_a_gap_still_exits_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    // Same registry, but the declared store does not exist: the harness probes
    // as missing, which is not a gap. A missing harness must not be dragged
    // into the new exit code — 3 has to mean "recognised but not archived".
    let db = sandbox.path().join("absent.db");
    assert!(!db.exists());

    let output = run_collect(sandbox.path(), &db);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "a pass with nothing recognised and nothing lost is still success;\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("[collect] not archivable  : 0 harness(es)"),
        "stdout={stdout}"
    );
}
