//! PASEO-1c: Codex identity compatibility.
//!
//! The Paseo handle UUID proves which rollout a Paseo agent refers to; the
//! archive identity stays the rollout filename stem. These tests pin that
//! existing archive ids do not change, against synthetic trees only (no
//! session bodies: every record is a fixture `session_meta`).

use chat_stasher::config::Config;
use chat_stasher::paseo::{AgentObservation, AgentProjection, BodyState};
use chat_stasher::paseo_codex::{
    archive_session_id, is_handle_uuid, resolve_rollout, CodexResolution, CodexRoot,
};
use chat_stasher::scanner;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const MACHINE: &str = "fixture-machine";
const UUID: &str = "019bf00d-0000-7000-8000-00000000000a";
const STEM: &str = "rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-00000000000a";
/// Literal, not computed: this string is what an archive written before
/// Paseo already holds on disk. If it ever has to change, existing archives
/// stop being found.
const PINNED_ARCHIVE_ID: &str =
    "codex.fixture-machine.rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-00000000000a";

fn meta_line(id: &str) -> Vec<u8> {
    format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n").into_bytes()
}

fn plant(home: &Path, root: &str, name: &str, body: &[u8]) -> PathBuf {
    let path = home.join(root).join("2026/09/01").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn zst(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 3).unwrap()
}

fn resolve(home: &Path) -> CodexResolution {
    resolve_rollout(home, UUID).unwrap()
}

fn resolved(home: &Path) -> chat_stasher::paseo_codex::CodexRollout {
    match resolve(home) {
        CodexResolution::Resolved(rollout) => rollout,
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// Scanner ids for a tree holding `~/.codex/sessions` and
/// `~/.codex/archived_sessions`, via the same registry shape the shipped one
/// uses. HOME is pinned for the duration.
fn scanner_ids(home: &Path, dir: &Path) -> Vec<String> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cell = |template: &str| {
        serde_json::json!({
            "template": template,
            "format": "jsonl / jsonl.zst",
            "confidence": "source-confirmed",
            "source": "synthetic PASEO-1c fixture"
        })
    };
    let platform = scanner::current_platform();
    let registry_path = dir.join("registry.json");
    fs::write(
        &registry_path,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "PASEO-1c synthetic",
            "harnesses": [{
                "id": "codex",
                "display_name": "synthetic codex",
                "paths": { platform: cell("~/.codex/sessions/") },
                "source_roots": [{
                    "id": "codex-archived",
                    "paths": { platform: cell("~/.codex/archived_sessions/") },
                    "movable": true
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let saved: Vec<_> = ["HOME", "USERPROFILE", "CODEX_HOME", scanner::REGISTRY_ENV]
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect();
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    std::env::remove_var("CODEX_HOME");
    std::env::set_var(scanner::REGISTRY_ENV, &registry_path);
    let result = scanner::scan_with_machine(&Config::default(), MACHINE);
    for (name, value) in saved {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    let mut ids: Vec<String> = result
        .unwrap()
        .records
        .iter()
        .map(|record| record.id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn pinned_archive_id_is_the_stem_not_the_uuid() {
    assert_eq!(archive_session_id(MACHINE, STEM), PINNED_ARCHIVE_ID);
    // The handle UUID alone would be a second identity for the same file.
    assert_ne!(archive_session_id(MACHINE, UUID), PINNED_ARCHIVE_ID);
}

#[test]
fn active_rollout_resolves_to_the_scanners_existing_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    let rollout = resolved(&home);
    assert_eq!(rollout.stem, STEM);
    assert_eq!(rollout.root, CodexRoot::Active);
    assert!(!rollout.compressed);

    // The scanner, run over a HOME holding the very same tree, produces the
    // pinned id; the resolver's id is that id.
    let fake_home = dir.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();
    plant(
        &fake_home.join(".codex"),
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    let scanned = scanner_ids(&fake_home, dir.path());
    assert_eq!(scanned, vec![PINNED_ARCHIVE_ID.to_string()]);
    assert_eq!(archive_session_id(MACHINE, &rollout.stem), scanned[0]);
}

#[test]
fn archived_root_and_compressed_rollouts_keep_the_same_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "archived_sessions",
        &format!("{STEM}.jsonl.zst"),
        &zst(&meta_line(UUID)),
    );
    let rollout = resolved(&home);
    assert_eq!(rollout.root, CodexRoot::Archived);
    assert!(rollout.compressed);
    assert_eq!(
        archive_session_id(MACHINE, &rollout.stem),
        PINNED_ARCHIVE_ID
    );

    let fake_home = dir.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();
    plant(
        &fake_home.join(".codex"),
        "archived_sessions",
        &format!("{STEM}.jsonl.zst"),
        &zst(&meta_line(UUID)),
    );
    assert_eq!(
        scanner_ids(&fake_home, dir.path()),
        vec![PINNED_ARCHIVE_ID.to_string()],
        "an archive move or compression must not change the id the scanner gives the file"
    );
}

#[test]
fn internal_uuid_that_disagrees_with_the_name_is_ambiguous_not_a_new_identity() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line("019bf00d-0000-7000-8000-00000000ffff"),
    );
    match resolve(&home) {
        CodexResolution::Ambiguous { reason } => {
            assert!(!reason.contains(UUID) && !reason.contains("ffff"));
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn one_stem_in_both_roots_is_ambiguous() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    plant(
        &home,
        "archived_sessions",
        &format!("{STEM}.jsonl.zst"),
        &zst(&meta_line(UUID)),
    );
    assert!(matches!(resolve(&home), CodexResolution::Ambiguous { .. }));
}

#[test]
fn absent_roots_and_other_sessions_are_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    assert_eq!(resolve(&home), CodexResolution::Missing);
    plant(
        &home,
        "sessions",
        "rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-00000000000b.jsonl",
        &meta_line("019bf00d-0000-7000-8000-00000000000b"),
    );
    assert_eq!(resolve(&home), CodexResolution::Missing);
}

#[test]
fn unreadable_first_records_are_unreadable_not_missing() {
    let name = format!("{STEM}.jsonl");
    for body in [
        b"not json\n".to_vec(),
        b"{\"type\":\"event_msg\",\"payload\":{}}\n".to_vec(),
        Vec::new(),
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("codex-home");
        plant(&home, "sessions", &name, &body);
        assert!(
            matches!(resolve(&home), CodexResolution::Unreadable { .. }),
            "body {body:?}"
        );
    }
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl.zst"),
        b"this is not zstd",
    );
    assert!(matches!(resolve(&home), CodexResolution::Unreadable { .. }));
}

// Symlink creation needs privileges on Windows, so the symlink arm cannot be
// planted there; the property itself (a link is never entered or read) is
// platform independent because the walk never follows file types it cannot
// classify as a plain file or directory.
#[cfg(unix)]
#[test]
fn a_symlinked_rollout_is_never_followed() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    let outside = dir.path().join("outside.jsonl");
    fs::write(&outside, meta_line(UUID)).unwrap();
    let link = home
        .join("sessions/2026/09/01")
        .join(format!("{STEM}.jsonl"));
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_eq!(resolve(&home), CodexResolution::Missing);
}

#[test]
fn handles_must_be_complete_lowercase_uuids() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(is_handle_uuid(UUID));
    for bad in [
        "019bf00d",
        "019BF00D-0000-7000-8000-00000000000A",
        STEM,
        "",
        "019bf00d-0000-7000-8000-00000000000",
    ] {
        assert!(!is_handle_uuid(bad));
        assert!(resolve_rollout(dir.path(), bad).is_err());
    }
}

#[test]
fn unproved_states_map_to_valid_privacy_safe_body_states() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    let path = plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    // Resolved is a located file, not an archived body.
    assert!(resolve(&home).unproved_body_state().is_none());

    fs::write(&path, b"not json\n").unwrap();
    let states = [
        CodexResolution::Missing,
        resolve(&home),
        CodexResolution::Ambiguous {
            reason: "two candidates".to_string(),
        },
    ];
    for state in states {
        let body = state.unproved_body_state().unwrap();
        body.validate().unwrap();
        assert!(!body.is_archived());
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains(STEM) && !text.contains(UUID), "{text}");
        assert!(!text.contains(home.to_str().unwrap()), "{text}");
    }
}

#[test]
fn an_observation_records_the_handle_and_leaves_the_archive_id_to_the_stem() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    let rollout = resolved(&home);
    let observation = AgentObservation {
        daemon_key: "fixture-daemon".to_string(),
        agent_id: "11111111-2222-4333-8444-555555555555".to_string(),
        provider: "codex".to_string(),
        native_session_id: Some(UUID.to_string()),
        projection: AgentProjection::default(),
        body: BodyState::Archived {
            canonical_session_id: archive_session_id(MACHINE, &rollout.stem),
            shard_sequences: vec![1],
            body_sha256: "0".repeat(64),
        },
    };
    observation.validate().unwrap();
    match &observation.body {
        BodyState::Archived {
            canonical_session_id,
            ..
        } => assert_eq!(canonical_session_id, PINNED_ARCHIVE_ID),
        _ => unreachable!(),
    }
}

/// Low-1 (PASEO-1c review): the `type == session_meta` check was not pinned;
/// removing it survived mutation. A first record of any other type that merely
/// carries `payload.id == handle` is not proof of identity.
#[test]
fn a_first_record_that_is_not_session_meta_is_not_identity_proof() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("codex-home");
    let impostor = format!("{{\"type\":\"event_msg\",\"payload\":{{\"id\":\"{UUID}\"}}}}\n");
    plant(
        &home,
        "sessions",
        &format!("{STEM}.jsonl"),
        impostor.as_bytes(),
    );
    assert!(
        matches!(resolve(&home), CodexResolution::Unreadable { .. }),
        "a non-session_meta first record must not resolve, got {:?}",
        resolve(&home)
    );

    // Positive control: the same bytes under type session_meta do resolve.
    let ok_home = dir.path().join("codex-home-ok");
    plant(
        &ok_home,
        "sessions",
        &format!("{STEM}.jsonl"),
        &meta_line(UUID),
    );
    assert!(matches!(resolve(&ok_home), CodexResolution::Resolved(_)));
}
