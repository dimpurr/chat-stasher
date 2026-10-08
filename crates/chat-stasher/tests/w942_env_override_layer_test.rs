//! W942 — an override's home layer is registry data, not a second table.
//!
//! `root_from_env_override` used to place the override by matching template
//! substrings against a hardcoded list (`Cursor/User/`, `.codex/`, `.gemini/`,
//! `.kimi-code/`, `.dsh/`) and returning `None` for anything else. That list was
//! a second registry: a harness added to `data/harness-registry-v1.json` with an
//! `env_override` it did not know resolved to the template instead, reading the
//! default location while the registry claimed the override worked — and no test
//! went red. The DeepSeek Harness pass proved the shape bites: deleting the
//! `.dsh/` arm made a real, shipped harness stop honouring `DSH_HOME`.
//!
//! The layer is now declared by the cell (`env_override_layer`), so a layer this
//! build has never heard of resolves. These tests hold the two halves of that:
//! the invariant that every shipped override cell declares the layer it replaces
//! (so forgetting one is red, not silent), and an end-to-end scan proving a
//! brand-new layer moves the scanned root. The resolver's own unit tests pin
//! every shipped harness's resolved `PathBuf` byte-for-byte.

use chat_stasher::config::Config;
use chat_stasher::scanner::{self, current_platform, HarnessRegistry};
use std::fs;
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const PLATFORMS: [&str; 3] = ["macos", "linux", "windows"];

/// The invariant the board asked for: every cell that exports an override names
/// the template layer that override replaces. A non-`OPENCODE_DB` override with
/// no declared layer is not honoured — the template decides — so this is the
/// check that makes "add a harness and forget its layer" go red instead of
/// quietly reading the default location.
#[test]
fn every_env_override_cell_declares_the_layer_it_replaces() {
    let registry = scanner::load_registry_from_repo().expect("shipped registry must load");
    let mut checked = 0usize;
    for h in &registry.harnesses {
        for platform in PLATFORMS {
            let Some(cell) = h.paths.cell_for(platform) else {
                continue;
            };
            let Some(variable) = cell.env_override.as_deref() else {
                continue;
            };
            if variable == "OPENCODE_DB" {
                // A database filename, not a directory layer: opencode's
                // override names the store itself, and its `:memory:` branch is
                // handled before any layer is consulted.
                assert!(
                    cell.env_override_layer.is_none(),
                    "{}.{platform} exports {variable}, which is a file, not a layer",
                    h.id
                );
                continue;
            }
            checked += 1;
            let layer = cell.env_override_layer.as_deref().unwrap_or_else(|| {
                panic!(
                    "{}.{platform} exports {variable} but declares no env_override_layer: the \
                     layer it replaces must live in the cell, or the override silently falls \
                     back to the template while the registry claims it works",
                    h.id
                )
            });
            assert!(
                !layer.is_empty(),
                "{}.{platform}: an empty layer names no directory",
                h.id
            );
            assert!(
                cell.template.replace('\\', "/").contains(layer),
                "{}.{platform}: {layer:?} is not a layer of template {:?}",
                h.id,
                cell.template
            );
        }
    }
    assert!(
        checked >= 15,
        "the shipped registry declares a layer for codex, gemini-cli, cursor, kimi-code and \
         deepseek-harness on every platform; checked={checked}"
    );
}

/// A one-harness registry whose cell names a layer this build never hardcoded:
/// `.w942-synthetic/`, reached through an equally new `W942_SYNTHETIC_HOME`.
/// The harness id is a real one so the build can label the records it finds —
/// the point under test is the layer, not the identity.
fn registry_for_this_platform() -> HarnessRegistry {
    let cell = serde_json::json!({
        "template": "~/.w942-synthetic/sessions/",
        "env_override": "W942_SYNTHETIC_HOME",
        "env_override_layer": ".w942-synthetic/",
        "format": "jsonl",
        "confidence": "measured-locally",
        "source": "W942 synthetic fixture"
    });
    let paths = match current_platform() {
        "macos" => serde_json::json!({ "macos": cell }),
        "linux" => serde_json::json!({ "linux": cell }),
        "windows" => serde_json::json!({ "windows": cell }),
        other => panic!("unexpected platform {other}"),
    };
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "generated": "W942 synthetic fixture",
        "harnesses": [{
            "id": "codex",
            "display_name": "Synthetic Codex",
            "paths": paths
        }]
    }))
    .expect("the synthetic registry is well formed")
}

/// The end-to-end half: a declared layer actually moves the scanned root, and
/// the store sitting there is counted — a resolved path over an empty walk
/// would only be half the fix.
#[test]
fn a_declared_layer_moves_the_scanned_root() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let override_home = temp.path().join("w942-home");
    let sessions = override_home.join("sessions");
    fs::create_dir_all(sessions.join("2026-10-08")).unwrap();
    fs::write(
        sessions.join("2026-10-08/019bf00d-0000.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )
    .unwrap();
    std::env::set_var("W942_SYNTHETIC_HOME", &override_home);

    let report = scanner::scan_with_registry_and_machine(
        &Config::default(),
        &registry_for_this_platform(),
        "synthetic",
    )
    .expect("the synthetic scan runs");

    let probe = report
        .probes
        .iter()
        .find(|probe| probe.id == "codex")
        .expect("the harness has a probe row");
    assert_eq!(
        probe.root.as_deref(),
        Some(sessions.as_path()),
        "the declared layer must be the resolved root (note: {})",
        probe.note
    );
    assert_ne!(
        probe.state,
        scanner::ProbeState::SkipUnresolvable,
        "a layer this build never hardcoded must still resolve: {}",
        probe.note
    );
    assert_eq!(
        probe.record_count,
        Some(1),
        "the rollout on disk must be counted, not left unknown (note: {})",
        probe.note
    );
    assert_eq!(report.records.len(), 1);

    std::env::remove_var("W942_SYNTHETIC_HOME");
}
