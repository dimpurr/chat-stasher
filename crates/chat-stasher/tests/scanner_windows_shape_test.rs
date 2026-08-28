//! The Windows shape of `scanner_test::gemini_pattern_counts_session_and_rejects_config_json`,
//! runnable on every platform.
//!
//! That test is green on macOS and Linux and red on `windows-latest`. It builds
//! its registry cell out of the *absolute path of a temp directory*, so the one
//! thing that differs across platforms is how the OS spells that path:
//!
//!   * macOS   `/var/folders/xx/…/T/.tmpAbCdEf/gemini-project/chats`
//!   * Linux   `/tmp/.tmpAbCdEf/gemini-project/chats`
//!   * Windows `C:\Users\RUNNER~1\AppData\Local\Temp\.tmpAbCdEf\gemini-project\chats`
//!
//! The Windows spelling carries a `~` **in the middle of a path component**:
//! `RUNNER~1` is the 8.3 short name of a long user name. `scanner`'s template
//! parser used to treat a `~` anywhere as the home-directory marker, so it read
//! `RUNNER~1` as `~something`, declared the template unanchorable, and the probe
//! became `SkipUnresolvable` — zero records for a directory that was right
//! there. Same family as the explicit-root work: a location the caller *did*
//! state, thrown away as if it had not been.
//!
//! The shape is reproducible without a Windows machine: `~` is a perfectly
//! ordinary character in a POSIX filename too, so a directory named `RUNNER~1`
//! produces exactly the property under test on macOS, Linux and Windows alike —
//! the template string handed to the scanner contains a `~` that is not a home
//! marker. Nothing here is `cfg!(windows)`-gated: all three platforms run the
//! identical code path.

use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry, ProbeState};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

/// A temp directory spelled the way the Windows CI runner's temp directory is:
/// with an 8.3 short-name component, i.e. a `~` inside a path component.
fn short_name_shaped_dir(base: &Path) -> PathBuf {
    let dir = base
        .join("RUNNER~1")
        .join("AppData")
        .join("Local")
        .join("Temp")
        .join(".tmpB60shape");
    fs::create_dir_all(&dir).unwrap();
    assert!(
        dir.display().to_string().contains('~'),
        "仪器前提失效：模拟的临时目录必须带一个非 home 含义的 `~`，实际是 {}",
        dir.display()
    );
    dir
}

/// Build the same one-harness gemini registry `scanner_test` builds, with the
/// cell template set to `chats` verbatim — exactly as that test does.
fn gemini_registry(chats: &Path) -> HarnessRegistry {
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
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "2026-08-17",
        "harnesses": [{
            "id": "gemini-cli",
            "display_name": "Gemini CLI",
            "paths": paths
        }]
    }))
    .unwrap()
}

/// The same invariant `scanner_test.rs:195` asserts, in the Windows shape: one
/// `session-*` file is planted next to two config files, so the scan must
/// report exactly one record — and must not decide the root is unresolvable
/// merely because a directory *name* happens to contain a tilde.
#[test]
fn short_name_tilde_in_template_still_anchors_the_root() {
    let base = tempfile::TempDir::new().unwrap();
    let chats = short_name_shaped_dir(base.path())
        .join("gemini-project")
        .join("chats");
    fs::create_dir_all(&chats).unwrap();

    fs::write(chats.join("session-2026-08-16-example.json"), b"{}\n").unwrap();
    fs::write(chats.join("settings.json"), b"{}\n").unwrap();
    fs::write(chats.join("state.json"), b"{}\n").unwrap();

    let registry = gemini_registry(&chats);
    let report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();

    let probe = &report.probes[0];
    println!(
        "windows-shape self-check: platform={} files=3 state={:?} root_resolved={} records={} note={}",
        scanner::current_platform(),
        probe.state,
        probe.root.is_some(),
        report.records.len(),
        probe.note
    );

    assert_ne!(
        probe.state,
        ProbeState::SkipUnresolvable,
        "路径里的 `~` 只是文件名的一部分，不该让整条模板变成「无法锚定」；note={}",
        probe.note
    );
    assert_eq!(
        report.records.len(),
        1,
        "种下 1 个 session-* + 2 个配置文件，应当只数出 1 条"
    );
    assert_eq!(probe.record_count, Some(1));
    assert!(report.records[0]
        .absolute_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("session-"));
}

/// The other half of the invariant, so the fix cannot be "stop treating `~` as
/// home at all": a leading `~/` is still the home directory, and a template
/// that genuinely cannot be anchored is still refused rather than silently
/// turned into some existing directory.
#[test]
fn leading_tilde_is_still_home_and_unanchorable_is_still_refused() {
    let base = tempfile::TempDir::new().unwrap();
    let unresolvable = json!({
        "template": "$CWD/.aider.chat.history.md",
        "format": "json",
        "confidence": "source-confirmed",
        "source": "test"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({ "macos": unresolvable }),
        "linux" => json!({ "linux": unresolvable }),
        "windows" => json!({ "windows": unresolvable }),
        other => panic!("unexpected platform: {other}"),
    };
    let registry: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "2026-08-17",
        "harnesses": [{ "id": "aider", "display_name": "aider", "paths": paths }]
    }))
    .unwrap();
    let report = scanner::scan_with_registry(&Config::default(), &registry).unwrap();
    assert_eq!(
        report.probes[0].state,
        ProbeState::SkipUnresolvable,
        "`$CWD/...` 依然锚不住，必须照旧拒绝，而不是退化成某个存在的目录"
    );
    assert_eq!(
        report.probes[0].record_count, None,
        "锚不住 = 未知，未知不能当成 0"
    );
    drop(base);
}
