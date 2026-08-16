//! Black-box tests for the scanner: build a scratch harness tree, scan it,
//! and assert on the metadata-only records. Nothing here touches real user
//! sessions — everything lives under a temp dir the test creates and removes.

use chat_stasher::config::Config;
use chat_stasher::models::HarnessSource;
use chat_stasher::scanner;
use std::fs;

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
    fs::write(codex_root.join(format!("{UUID_B}.jsonl.zst")), vec![0u8; 64]).unwrap();

    let report = scanner::scan(&config).unwrap();

    assert_eq!(report.records.len(), 3);
    assert!(report.missing_roots.is_empty());

    let mut claude = report.records.iter().filter(|r| r.source == HarnessSource::ClaudeCode);
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
    let zname = z.absolute_path.file_name().unwrap().to_string_lossy().to_string();
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
    let report = scanner::scan(&config).unwrap();
    assert!(report.records.is_empty());
    assert_eq!(report.missing_roots.len(), 2);
    drop(dir);
}

#[test]
fn scan_strips_zst_suffix_into_native_uuid() {
    let (dir, config) = scratch();
    let codex_root = dir.path().join(".codex").join("sessions");
    let nested = codex_root.join("2026-05-01");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join(format!("{UUID_A}.jsonl.zst")), vec![0u8; 10]).unwrap();

    let report = scanner::scan(&config).unwrap();
    assert_eq!(report.records.len(), 1);
    let rec = &report.records[0];
    assert!(rec.id.ends_with(&format!(".{UUID_A}")));
    assert!(!rec.id.contains(".jsonl"));
    drop(dir);
}