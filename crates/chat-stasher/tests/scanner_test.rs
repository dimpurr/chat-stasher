//! Black-box tests for the scanner: build a scratch harness tree, scan it
//! against a scratch registry, and assert on the metadata-only records.
//! Nothing here touches real user sessions or the repo's registry — everything
//! lives under a temp dir the test creates and removes.

use chat_stasher::config::Config;
use chat_stasher::models::HarnessSource;
use chat_stasher::scanner::{self, HarnessRegistry, ProbeState};
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Build a registry with just claude-code + codex, every platform cell
/// populated (so the test passes regardless of which OS it runs on).
fn scratch_registry() -> HarnessRegistry {
    let cell = |template: &str| json!({ "template": template, "format": "jsonl / jsonl.zst", "confidence": "source-confirmed", "source": "test" });
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "2026-08-16",
        "harnesses": [
            {
                "id": "claude-code",
                "display_name": "Claude Code",
                "paths": {
                    "macos": cell("~/.claude/projects"),
                    "linux": cell("~/.claude/projects"),
                    "windows": cell("%USERPROFILE%\\.claude\\projects"),
                }
            },
            {
                "id": "codex",
                "display_name": "OpenAI Codex CLI",
                "paths": {
                    "macos": cell("~/.codex/sessions/"),
                    "linux": cell("~/.codex/sessions/"),
                    "windows": cell("%USERPROFILE%\\.codex\\sessions\\"),
                }
            }
        ]
    }))
    .expect("scratch registry must deserialize")
}

/// Create a scratch `~/.claude/projects` + `~/.codex/sessions` pair that the
/// test can then point the scanner at via config overrides.
fn scratch() -> (tempfile::TempDir, Config) {
    let dir = tempfile::TempDir::new().expect("make tempdir");
    let claude_root = dir.path().join(".claude").join("projects");
    let codex_root = dir.path().join(".codex").join("sessions");
    fs::create_dir_all(&claude_root).unwrap();
    fs::create_dir_all(&codex_root).unwrap();

    let config = Config {
        claude_projects_dir: Some(claude_root.to_string_lossy().to_string()),
        codex_sessions_dir: Some(codex_root.to_string_lossy().to_string()),
        ..Default::default()
    };
    (dir, config)
}

const UUID_A: &str = "019bf00d-97b6-7eb2-9bf8-eacbacc09765";
const UUID_B: &str = "11c2aa3f-0000-4a6b-8a44-014f48d8c1e1";

#[test]
fn scan_indexes_both_harnesses_and_counts_compressed() {
    let (dir, config) = scratch();
    let claude_root = dir.path().join(".claude").join("projects");
    let codex_root = dir.path().join(".codex").join("sessions");

    // One claude session, two codex sessions (one of them zst-compressed).
    fs::write(claude_root.join(format!("{UUID_A}.jsonl")), b"{}\n").unwrap();
    fs::write(codex_root.join(format!("{UUID_A}.jsonl")), b"{}\n{}\n").unwrap();
    fs::write(
        codex_root.join(format!("{UUID_B}.jsonl.zst")),
        vec![0u8; 64],
    )
    .unwrap();

    let registry = scratch_registry();
    let report = scanner::scan_with_registry(&config, &registry).unwrap();

    assert_eq!(report.records.len(), 3);
    assert!(report.missing_roots.is_empty());
    assert_eq!(report.probes.len(), 2);

    let mut claude = report
        .records
        .iter()
        .filter(|r| r.source == HarnessSource::ClaudeCode);
    let c = claude.next().unwrap();
    assert_eq!(c.byte_size, 3);
    assert!(!c.compressed);
    assert!(c.id.starts_with("claude-code."));
    assert!(c.id.ends_with(&format!(".{UUID_A}")));

    let codex: Vec<_> = report
        .records
        .iter()
        .filter(|r| r.source == HarnessSource::Codex)
        .collect();
    assert_eq!(codex.len(), 2);
    let idx = codex
        .iter()
        .position(|r| r.compressed)
        .expect("zst file must be listed, never skipped");
    let z = codex[idx];
    let zname = z
        .absolute_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(zname.ends_with(".jsonl.zst"));
    assert_eq!(z.byte_size, 64);
    assert!(z.id.ends_with(&format!(".{UUID_B}")));

    drop(dir);
}

#[test]
fn scan_reports_missing_root_instead_of_failing() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = Config {
        claude_projects_dir: Some(dir.path().join("no-such-dir").to_string_lossy().into()),
        codex_sessions_dir: Some(dir.path().join("no-such-either").to_string_lossy().into()),
        ..Default::default()
    };
    let registry = scratch_registry();
    let report = scanner::scan_with_registry(&config, &registry).unwrap();
    assert!(report.records.is_empty());
    assert_eq!(report.missing_roots.len(), 2);
    assert!(report.probes.iter().all(|p| p.state == ProbeState::Missing));
    drop(dir);
}

#[test]
fn scan_strips_zst_suffix_into_native_uuid() {
    let (dir, config) = scratch();
    let codex_root = dir.path().join(".codex").join("sessions");
    let nested = codex_root.join("2026-05-01");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join(format!("{UUID_A}.jsonl.zst")), vec![0u8; 10]).unwrap();

    let registry = scratch_registry();
    let report = scanner::scan_with_registry(&config, &registry).unwrap();
    assert_eq!(report.records.len(), 1);
    let rec = &report.records[0];
    assert!(rec.id.ends_with(&format!(".{UUID_A}")));
    assert!(!rec.id.contains(".jsonl"));
    drop(dir);
}

#[test]
fn gemini_pattern_counts_session_and_rejects_config_json() {
    let dir = tempfile::TempDir::new().unwrap();
    let chats = dir.path().join("gemini-project").join("chats");
    fs::create_dir_all(&chats).unwrap();

    fs::write(chats.join("session-2026-08-16-example.json"), b"{}\n").unwrap();
    fs::write(chats.join("settings.json"), b"{}\n").unwrap();
    fs::write(chats.join("state.json"), b"{}\n").unwrap();

    let cell = json!({
        "template": chats.to_string_lossy(),
        "format": "json",
        "session_pattern": "session-*",
        "confidence": "source-confirmed",
        "source": "test"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({ "macos": cell }),
        "linux" => json!({ "linux": cell }),
        "windows" => json!({ "windows": cell }),
        other => panic!("unexpected platform: {other}"),
    };
    let registry: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "2026-08-16",
        "harnesses": [{
            "id": "gemini-cli",
            "display_name": "Gemini CLI",
            "paths": paths
        }]
    }))
    .unwrap();

    let report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();
    println!(
        "negative self-check: files=3, registry session records={}",
        report.records.len()
    );
    assert_eq!(report.records.len(), 1);
    assert_eq!(report.probes[0].record_count, Some(1));
    assert!(report.records[0]
        .absolute_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("session-"));
}

#[test]
fn opencode_db_env_override_wins_for_scanner() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env_home = tempfile::TempDir::new().unwrap();
    let xdg_data = tempfile::TempDir::new().unwrap();
    let home_fallback = tempfile::TempDir::new().unwrap();
    let plant_db = |db: &std::path::Path| {
        if let Some(parent) = db.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL); INSERT INTO session VALUES ('fixture', 1760000000000, 1760000000000);",
        )
        .unwrap();
    };
    let db = env_home.path().join("opencode.db");
    plant_db(&db);

    std::env::set_var("OPENCODE_DB", &db);
    std::env::set_var("XDG_DATA_HOME", xdg_data.path());
    let cell = json!({
        "template": "$XDG_DATA_HOME/opencode/opencode.db",
        "env_override": "OPENCODE_DB",
        "format": "sqlite",
        "confidence": "source-confirmed",
        "source": "test",
        "sql_table": "session",
        "sql_required_columns": ["id", "time_created", "time_updated"],
        "sql_time_column": "time_created"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({ "macos": cell }),
        "linux" => json!({ "linux": cell }),
        "windows" => json!({ "windows": cell }),
        other => panic!("unexpected platform: {other}"),
    };
    let registry: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "2026-08-17",
        "harnesses": [{
            "id": "opencode",
            "display_name": "opencode",
            "paths": paths
        }]
    }))
    .unwrap();

    let report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();
    let probe = &report.probes[0];
    println!(
        "opencode_env_override_selected={} record_count={}",
        probe.root.as_deref() == Some(db.as_path()),
        probe.record_count.unwrap_or(0)
    );
    assert_eq!(probe.root.as_deref(), Some(db.as_path()));
    assert_eq!(probe.state, ProbeState::FileTarget);
    assert_eq!(probe.record_count, Some(1));

    let xdg_db = xdg_data.path().join("opencode/opencode.db");
    plant_db(&xdg_db);
    std::env::remove_var("OPENCODE_DB");
    let xdg_report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();
    let xdg_probe = &xdg_report.probes[0];
    assert_eq!(xdg_probe.root.as_deref(), Some(xdg_db.as_path()));
    assert_eq!(xdg_probe.record_count, Some(1));

    let home_db = home_fallback
        .path()
        .join(".local/share/opencode/opencode.db");
    plant_db(&home_db);
    std::env::remove_var("XDG_DATA_HOME");
    std::env::set_var("HOME", home_fallback.path());
    let home_report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();
    let home_probe = &home_report.probes[0];
    println!(
        "opencode_fallbacks=OPENCODE_DB:{} XDG_DATA_HOME:{} HOME/.local/share:{}",
        probe.root.as_deref() == Some(db.as_path()),
        xdg_probe.root.as_deref() == Some(xdg_db.as_path()),
        home_probe.root.as_deref() == Some(home_db.as_path())
    );
    assert_eq!(home_probe.root.as_deref(), Some(home_db.as_path()));
    assert_eq!(home_probe.record_count, Some(1));

    std::env::remove_var("OPENCODE_DB");
    std::env::remove_var("XDG_DATA_HOME");
}
