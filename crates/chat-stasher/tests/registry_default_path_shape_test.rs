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
//! `data/harness-registry-v1.json` and planted in all three platform slots of a
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
        "仪器前提：这台机器不许有配置文件"
    );
}

/// A scratch registry carrying `template` (the shipped cell for some platform)
/// in all three platform slots, plus Cursor's shipped SQL schema.
fn plant_registry(home: &Path, template: &str) -> PathBuf {
    let escaped = template.replace('\\', "\\\\").replace('"', "\\\"");
    let cell = format!(
        r#"{{ "template": "{escaped}",
              "env_override": "CURSOR_USER_DIR", "format": "sqlite",
              "confidence": "仅社区说法未核实", "source": "B59 test: 出货 registry 的 cursor 格",
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
        "{shape} 形状、没有任何显式配置时，cursor 模板必须能解析出根路径；\
         实际 state={:?} note={} template={template}",
        probe.state,
        probe.note
    );
    let root = probe.root.as_ref().unwrap_or_else(|| {
        panic!(
            "{shape} 形状下 cursor 必须解析出根路径，实际 root=None (note={})",
            probe.note
        )
    });
    let shown = root.to_string_lossy().to_string();

    assert!(
        root.starts_with(home),
        "{shape}: 解析出的根路径必须落在隔离 HOME 内，实际 {shown}"
    );
    assert!(
        shown.ends_with("state.vscdb"),
        "{shape}: 根路径必须指向 Cursor 的全局库 state.vscdb，实际 {shown}"
    );
    assert!(
        shown.contains("globalStorage"),
        "{shape}: 根路径必须经过 globalStorage 这一层，实际 {shown}"
    );
    assert!(
        !shown.contains('%'),
        "{shape}: 根路径里还留着未展开的 %VAR%：{shown}"
    );
    assert!(
        !shown.contains('（') && !shown.contains('）'),
        "{shape}: 根路径里混进了模板的中文注释尾巴：{shown}"
    );

    // The other half, and the one that must never be relaxed: the store is not
    // there, so "how many sessions" stays unknown. Anchoring a path is not
    // looking at one.
    assert!(!root.exists(), "仪器前提：这台机器上不该真有 {shown}");
    assert_eq!(
        probe.record_count, None,
        "{shape}: 目录不存在时会话数必须是「未知」，不是 {:?}",
        probe.record_count
    );
    assert!(
        !probe.installed_p(),
        "{shape}: 目录不存在时不得判为已安装（state={:?}）",
        probe.state
    );
    let fp = report
        .footprints
        .iter()
        .find(|f| f.name == "cursor")
        .expect("cursor footprint row must exist");
    assert_eq!(
        fp.session_count, None,
        "{shape}: 目录不存在时 footprint 会话数必须是「未知」，不是 {:?}",
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
        .unwrap_or_else(|| panic!("cursor 必须解析出根路径 (note={})", probe.note));
    assert!(
        root.starts_with(&roaming),
        "导出的 %APPDATA% 必须优先于回退值，实际 {}",
        root.display()
    );
    assert_eq!(probe.record_count, None, "目录不存在，会话数仍是「未知」");

    std::env::remove_var("APPDATA");
    std::env::remove_var(scanner::REGISTRY_ENV);
}
