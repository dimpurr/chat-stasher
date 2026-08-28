//! Both directions of "do not print a number you did not earn".
//!
//! B55 fixed one direction: a footprint row must not say `Some(0)` about a
//! directory no probe ever opened — `Some(0)` claims "I enumerated it and it is
//! empty". It fixed that by making the registry probe the sole source of the
//! footprint row's count.
//!
//! That leaves the mirror-image lie available: saying `None` ("I could not
//! determine") about a store the user pointed us at, that we opened, and whose
//! rows we counted. The registry's per-platform template and its `confidence`
//! gate exist to stop the scanner walking a path **we** guessed. A path the
//! user wrote in `[harness_roots]` is not a guess, so neither gate applies to
//! it — but a configured path that is not there still yields "unknown", never
//! `0`.
//!
//! Both tests here run against an isolated temporary HOME **and a scratch
//! registry written by the test itself**, whose cells are identical for macos /
//! linux / windows. They therefore take the same code path on a machine with
//! every harness installed and on a bare CI runner, and on every platform.

use chat_stasher::doctor::{self, HarnessFootprint};
use chat_stasher::scanner::{self, HarnessProbe};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point every base-directory variable this build consults at `home`, so no
/// probe can escape into the real machine.
fn isolate_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
    std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
    for var in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
    ] {
        std::env::remove_var(var);
    }
}

/// Write the config the tool will load (`$XDG_CONFIG_HOME/chat-stasher/config.toml`).
fn write_config(home: &Path, body: &str) {
    let path = home.join("xdg-config/chat-stasher/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
}

/// A one-harness registry whose codex cell is `未查明` on **every** platform —
/// the shape the shipped registry carries for grok on linux: a location we have
/// no verified template for, so we refuse to guess one.
///
/// All three cells carry it on purpose. A registry that only filled in `linux`
/// would make the test silently stop testing on macOS, which is exactly how the
/// original divergence reached CI green on macOS and red on Linux.
fn write_unascertained_registry(home: &Path) {
    let cell = r#"{ "template": "$NOWHERE_AT_ALL/sessions/",
                    "format": "jsonl", "confidence": "unascertained",
                    "source": "B57 test: 未核实" }"#;
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B57",
                  "harnesses": [
                    {{ "id": "codex", "display_name": "OpenAI Codex CLI",
                       "paths": {{ "macos": {cell}, "linux": {cell}, "windows": {cell} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var("CHAT_STASHER_REGISTRY", &path);
}

fn footprint<'a>(report: &'a doctor::DoctorReport, name: &str) -> &'a HarnessFootprint {
    report
        .footprints
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("footprint row missing for {name}"))
}

fn probe<'a>(report: &'a doctor::DoctorReport, id: &str) -> &'a HarnessProbe {
    report
        .probes
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("registry probe row missing for {id}"))
}

/// Direction 1 (the B55 direction, kept nailed down): the registry has no
/// usable cell, the config names no path, and nothing is on disk. Nobody ever
/// looked, so the only honest cell is "unknown" — `Some(0)` here would claim an
/// enumeration that never happened.
#[test]
fn no_registry_cell_no_config_and_no_directory_is_unknown_not_zero() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_home(home.path());
    write_unascertained_registry(home.path());
    // No config file at all: nothing tells the tool where codex lives.

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    // Premise: the directory really is absent, so "unknown" is not covering up
    // a store we could have counted.
    assert!(
        !home.path().join(".codex/sessions").exists(),
        "测试自身的前提：目录必须不存在"
    );

    let pr = probe(&report, "codex");
    assert_eq!(
        pr.state,
        scanner::ProbeState::SkipUnascertained,
        "前提失效：未查明的格本应被跳过（note={}）",
        pr.note
    );
    assert_eq!(pr.record_count, None, "probe 侧必须是「未能确定」");

    let fp = footprint(&report, "codex");
    assert_eq!(
        fp.session_count, None,
        "没人看过这个位置，footprint 不得宣称会话数 {:?}（0 = 「我数过，是空的」）",
        fp.session_count
    );
    assert!(!fp.installed, "没人看过，就不能判为已安装");

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}

/// Direction 2 (this task): the exact same registry — no usable cell for any
/// platform — but the user wrote the path in `[harness_roots]`, the directory
/// is there, and it holds 2 sessions. Reporting "unknown" would be a false
/// claim in the mirror direction: we did look, and we did count.
#[test]
fn config_declared_root_with_sessions_reports_the_real_count() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_home(home.path());
    write_unascertained_registry(home.path());

    const PLANTED: u64 = 2;
    let root = home.path().join("elsewhere/codex-sessions");
    for i in 0..PLANTED {
        let session = root.join(format!("2026-08-01/019bf00d-{i:04}.jsonl"));
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        fs::write(&session, "{}\n").unwrap();
    }
    write_config(
        home.path(),
        &format!(
            "[harness_roots]\ncodex = \"{}\"\n",
            root.display().to_string().replace('\\', "\\\\")
        ),
    );

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    // Premise: the store really is there and really is non-empty.
    assert!(root.is_dir(), "测试自身的前提：配置指的目录确实存在");

    let pr = probe(&report, "codex");
    assert_eq!(
        pr.state,
        scanner::ProbeState::Scanned,
        "配置显式指了路径就必须真的去走它（note={}）",
        pr.note
    );
    assert_eq!(
        pr.root.as_deref(),
        Some(root.as_path()),
        "probe 必须落在配置给的路径上"
    );
    assert_eq!(
        pr.record_count,
        Some(PLANTED),
        "probe 侧应数出种下的 {PLANTED} 个会话"
    );

    let fp = footprint(&report, "codex");
    assert_eq!(
        fp.session_count,
        Some(PLANTED),
        "配置指了路径、位置存在、也真的数出来了 —— 报「不知道」同样是不实"
    );
    assert!(fp.installed, "真的走过并数出来了，就是已安装");

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}
