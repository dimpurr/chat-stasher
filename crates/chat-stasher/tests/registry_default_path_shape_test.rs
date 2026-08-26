//! The "user wrote no configuration at all" case, in the Linux and the Windows
//! shape, runnable on every platform.
//!
//! `doctor_windows_shape_test` proves the *configured* path is honoured on a
//! Windows-shaped machine. This file asks the question one layer earlier: on a
//! machine where the user has configured **nothing** — no `config.toml`, no
//! `CURSOR_USER_DIR` — does the shipped registry template alone anchor Cursor's
//! store?
//!
//! Both shapes are reproduced with the same instrument the Windows shape test
//! uses: the *shipped* cell for the foreign platform is read out of
//! `crates/chat-stasher/data/harness-registry-v1.json` and planted in all
//! three platform slots of a
//! scratch registry, so the foreign platform's template travels the identical
//! code path on macOS, Linux and Windows.
//!
//! What is asserted is only that the template **anchors** — the resolved root
//! must be a real path under the isolated HOME, ending at the store. Whether
//! anything is *there* is a separate question, and the last assertion of each
//! test pins the other half down: the directory does not exist, so the session
//! count stays `None`. Resolving a path is not the same as having looked.

use chat_stasher::doctor;
use chat_stasher::scanner::{self, HarnessProbe};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Read one shipped cell straight out of the registry this build ships, so the
/// test can never drift from the data file it is about.
fn shipped_cursor_template(platform: &str) -> String {
    std::env::remove_var(scanner::REGISTRY_ENV);
    let registry = scanner::load_registry_from_repo().expect("shipped registry must load");
    let cursor = registry
        .harnesses
        .iter()
        .find(|h| h.id == "cursor")
        .expect("shipped registry must carry cursor");
    cursor
        .paths
        .cell_for(platform)
        .unwrap_or_else(|| panic!("shipped registry must carry cursor.{platform}"))
        .template
        .clone()
}

/// A foreign platform's template, spelled the way *this* platform spells a
/// path.
///
/// On Windows `\` is the path separator and `Path::parent()` splits on it; on
/// macOS/Linux it is an ordinary filename character. Simulating the Windows
/// shape therefore has to translate the separator, exactly as
/// `doctor_windows_shape_test` has to *introduce* backslashes to simulate a
/// Windows home. What the translation must not touch — and does not — is the
/// `%APPDATA%` prefix, which is the thing under test.
fn separators_for_this_platform(template: &str) -> String {
    if cfg!(windows) {
        template.to_string()
    } else {
        template.replace('\\', "/")
    }
}

/// Point every base-directory variable at `home` and clear every override, so
/// the run is what a brand-new machine with no configuration looks like: only
/// the registry template can supply a path.
fn isolate_unconfigured_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    for var in [
        // No XDG value set: the `~/.config` / `~/.local/share` fallbacks are
        // themselves part of "nothing configured".
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        // Windows app-data bases, likewise unset.
        "APPDATA",
        "LOCALAPPDATA",
        // Harness-specific overrides would make this a *configured* machine.
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
    ] {
        std::env::remove_var(var);
    }
    assert!(
        !home.join(".config/chat-stasher/config.toml").exists(),
        "instrument premise: this machine must have no config file"
    );
}

/// A scratch registry carrying `template` (the shipped cell for some platform)
/// in all three platform slots, plus Cursor's shipped SQL schema.
fn plant_registry(home: &Path, template: &str) -> PathBuf {
    let escaped = template.replace('\\', "\\\\").replace('"', "\\\"");
    let cell = format!(
        r#"{{ "template": "{escaped}",
              "env_override": "CURSOR_USER_DIR", "format": "sqlite",
              "confidence": "仅社区说法未核实", "source": "B59 test: the shipped registry's cursor cell",
              "sql_table": "cursorDiskKV", "sql_id_column": "key",
              "sql_required_columns": ["key", "value"],
              "sql_key_column": "key", "sql_key_pattern": "composerData:%",
              "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
              "sql_qualification": "cursor_composer" }}"#
    );
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B59",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cell}, "linux": {cell}, "windows": {cell} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var(scanner::REGISTRY_ENV, &path);
    path
}

/// The shared invariant: the template alone anchored a root under this HOME,
/// pointing at Cursor's global store — and nothing was claimed about what is
/// inside it.
fn assert_anchored_but_not_counted(
    report: &doctor::DoctorReport,
    home: &Path,
    template: &str,
    shape: &str,
) {
    let probe: &HarnessProbe = report
        .probes
        .iter()
        .find(|p| p.id == "cursor")
        .expect("cursor probe row must exist");

    assert_ne!(
        probe.state,
        scanner::ProbeState::SkipUnresolvable,
        "{shape} shape with no explicit config: the cursor template must resolve to a root path; \
         actual state={:?} note={} template={template}",
        probe.state,
        probe.note
    );
    let root = probe.root.as_ref().unwrap_or_else(|| {
        panic!(
            "{shape} shape: cursor must resolve a root path, actual root=None (note={})",
            probe.note
        )
    });
    let shown = root.to_string_lossy().to_string();

    assert!(
        root.starts_with(home),
        "{shape}: resolved root must land inside the isolated HOME, actual {shown}"
    );
    assert!(
        shown.ends_with("state.vscdb"),
        "{shape}: root must point at Cursor's global store state.vscdb, actual {shown}"
    );
    assert!(
        shown.contains("globalStorage"),
        "{shape}: root must go through the globalStorage layer, actual {shown}"
    );
    assert!(
        !shown.contains('%'),
        "{shape}: an unexpanded %VAR% remains in the root path: {shown}"
    );
    assert!(
        !shown.contains('（') && !shown.contains('）'),
        "{shape}: the template's Chinese annotation tail leaked into the root path: {shown}"
    );

    // The other half, and the one that must never be relaxed: the store is not
    // there, so "how many sessions" stays unknown. Anchoring a path is not
    // looking at one.
    assert!(
        !root.exists(),
        "instrument premise: this store must not really exist on this machine ({shown})"
    );
    assert_eq!(
        probe.record_count, None,
        "{shape}: when the directory is absent the session count must be \"unknown\", not {:?}",
        probe.record_count
    );
    assert!(
        !probe.installed_p(),
        "{shape}: an absent directory must not be judged installed (state={:?})",
        probe.state
    );
    let fp = report
        .footprints
        .iter()
        .find(|f| f.name == "cursor")
        .expect("cursor footprint row must exist");
    assert_eq!(
        fp.session_count, None,
        "{shape}: when the directory is absent the footprint session count must be \"unknown\", not {:?}",
        fp.session_count
    );
}

/// Linux shape: `$XDG_CONFIG_HOME/Cursor/User/globalStorage/state.vscdb` with
/// no `XDG_CONFIG_HOME` set — i.e. the `~/.config` fallback, the path a Linux
/// user who configured nothing actually has.
#[test]
fn linux_shape_without_any_config_still_anchors_cursor() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let template = separators_for_this_platform(&shipped_cursor_template("linux"));
    plant_registry(home.path(), &template);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");
    assert_anchored_but_not_counted(&report, home.path(), &template, "linux");

    std::env::remove_var(scanner::REGISTRY_ENV);
}

/// Windows shape: `%APPDATA%\Cursor\User\globalStorage\state.vscdb` with no
/// `APPDATA` set — the `%USERPROFILE%\AppData\Roaming` default. On a real
/// Windows box `APPDATA` is always set; the unset case is the stricter one, so
/// it is what the test runs.
#[test]
fn windows_shape_without_any_config_still_anchors_cursor() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let template = separators_for_this_platform(&shipped_cursor_template("windows"));
    plant_registry(home.path(), &template);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");
    assert_anchored_but_not_counted(&report, home.path(), &template, "windows");

    std::env::remove_var(scanner::REGISTRY_ENV);
}

/// And when `%APPDATA%` *is* exported — the ordinary Windows case — the
/// exported value wins over the fallback.
#[test]
fn windows_shape_honours_an_exported_appdata() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let roaming = home.path().join("Roaming");
    std::env::set_var("APPDATA", &roaming);
    let template = separators_for_this_platform(&shipped_cursor_template("windows"));
    plant_registry(home.path(), &template);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "cursor")
        .expect("cursor probe row must exist");
    let root = probe
        .root
        .as_ref()
        .unwrap_or_else(|| panic!("cursor must resolve a root path (note={})", probe.note));
    assert!(
        root.starts_with(&roaming),
        "an exported %APPDATA% must win over the fallback, actual {}",
        root.display()
    );
    assert_eq!(
        probe.record_count, None,
        "directory absent, session count stays \"unknown\""
    );

    std::env::remove_var("APPDATA");
    std::env::remove_var(scanner::REGISTRY_ENV);
}
