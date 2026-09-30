//! W285 §1 — the shipped registry's Codex cell on Linux and Windows never
//! resolved a root, so on such a machine every Codex session was invisible:
//! the probe reported `skip(template)` / `sessions=unknown` whether or not
//! `CODEX_HOME` was exported, and no rollout was ever archived.
//!
//! The reproduction is deliberately platform-independent. `~/.codex/sessions/`
//! — the macOS cell — always resolved, so a test that asked *this* machine's
//! `current_platform()` cell would be green on macOS and would only ever be red
//! on the platform the report came from. `doctor_empty_machine_test` names that
//! trap ("a registry that only fills in `linux` would make this test silently
//! stop testing on macOS"). These tests therefore read the *shipped* foreign
//! cell out of `crates/chat-stasher/data/harness-registry-v1.json` and plant it
//! in all three platform slots of a scratch registry, so the foreign template
//! travels the identical code path here.
//!
//! What is asserted is the whole §1 claim, both halves of it: the cell anchors
//! at the documented default when `CODEX_HOME` is unset, the exported
//! `CODEX_HOME` wins when it is set, and in both cases the rollout sitting
//! there is genuinely *counted* — a resolved path with an uncounted directory
//! would only be half the fix.

use chat_stasher::doctor;
use chat_stasher::scanner::{self, HarnessProbe};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Read one shipped cell straight out of the registry this build ships, so the
/// test can never drift from the data file it is about.
fn shipped_codex_template(platform: &str) -> String {
    std::env::remove_var(scanner::REGISTRY_ENV);
    let registry = scanner::load_registry_from_repo().expect("shipped registry must load");
    let codex = registry
        .harnesses
        .iter()
        .find(|h| h.id == "codex")
        .expect("shipped registry must carry codex");
    codex
        .paths
        .cell_for(platform)
        .unwrap_or_else(|| panic!("shipped registry must carry codex.{platform}"))
        .template
        .clone()
}

/// Spell a foreign platform's template the way *this* platform spells a path.
///
/// On Windows `\` separates path components and the resolver's own `.codex/`
/// matching runs after a `\`→`/` fold; on macOS/Linux a backslash is an
/// ordinary filename character, so simulating the Windows shape has to
/// translate the separator — the same technique
/// `registry_default_path_shape_test` uses for Cursor. What the translation must
/// not touch, and does not, is the `%USERPROFILE%` prefix under test.
fn separators_for_this_platform(template: &str) -> String {
    if cfg!(windows) {
        template.to_string()
    } else {
        template.replace('\\', "/")
    }
}

/// A brand-new machine: every base-directory variable points at `home` and
/// every harness override is cleared, so only the registry template can supply
/// a path.
fn isolate_unconfigured_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    for var in [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "APPDATA",
        "LOCALAPPDATA",
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

/// A scratch registry carrying `template` in all three platform slots, holding
/// codex's real override variable.
fn plant_codex_registry(home: &Path, template: &str) -> PathBuf {
    let escaped = template.replace('\\', "\\\\").replace('"', "\\\"");
    let cell = format!(
        r#"{{ "template": "{escaped}",
              "env_override": "CODEX_HOME", "format": "jsonl / jsonl.zst",
              "confidence": "source-confirmed", "source": "W285 test: the shipped registry's codex cell" }}"#
    );
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "W285",
                  "harnesses": [
                    {{ "id": "codex", "display_name": "OpenAI Codex CLI",
                       "paths": {{ "macos": {cell}, "linux": {cell}, "windows": {cell} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var(scanner::REGISTRY_ENV, &path);
    path
}

/// One real rollout where the harness keeps them: `<root>/<date>/<id>.jsonl`.
fn plant_rollout(sessions_root: &Path) {
    let rollout = sessions_root.join("2026-09-01/019bf00d-0000.jsonl");
    fs::create_dir_all(rollout.parent().expect("rollout has a parent")).expect("create date dir");
    fs::write(&rollout, "{\"type\":\"session_meta\"}\n").expect("write rollout");
}

fn codex_probe(report: &doctor::DoctorReport) -> &HarnessProbe {
    report
        .probes
        .iter()
        .find(|p| p.id == "codex")
        .expect("codex probe row must exist")
}

/// The shared invariant: the foreign cell alone anchored codex at
/// `expected_root`, and the rollout sitting there was counted.
fn assert_anchored_and_counted(
    report: &doctor::DoctorReport,
    expected_root: &Path,
    shape: &str,
    template: &str,
) {
    let probe = codex_probe(report);

    assert_ne!(
        probe.state,
        scanner::ProbeState::SkipUnresolvable,
        "{shape} shape: the codex template must resolve to a root path; \
         actual state={:?} note={} template={template}",
        probe.state,
        probe.note
    );
    let root = probe.root.as_ref().unwrap_or_else(|| {
        panic!(
            "{shape} shape: codex must resolve a root path, actual root=None (note={})",
            probe.note
        )
    });
    assert_eq!(
        root,
        expected_root,
        "{shape} shape: codex must anchor at {}, actual {}",
        expected_root.display(),
        root.display()
    );
    assert!(
        probe.installed_p(),
        "{shape} shape: codex must be judged installed once its store is there; actual state={:?}",
        probe.state
    );
    assert_eq!(
        probe.record_count,
        Some(1),
        "{shape} shape: the rollout on disk must be counted, not left unknown; actual state={:?} note={}",
        probe.state,
        probe.note
    );

    let fp = report
        .footprints
        .iter()
        .find(|f| f.name == "codex")
        .expect("codex footprint row must exist");
    assert_eq!(
        fp.session_count,
        Some(1),
        "{shape} shape: the footprint table must agree with the probe it derives from"
    );
    assert!(
        fp.installed,
        "{shape} shape: the footprint table must agree with the probe it derives from"
    );
}

/// The Linux shape with `CODEX_HOME` unset: the documented default
/// `$HOME/.codex/sessions/`, which is what a Linux user who configured nothing
/// actually has.
#[test]
fn linux_shape_without_codex_home_reaches_the_documented_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let sessions = home.path().join(".codex/sessions");
    plant_rollout(&sessions);
    let template = separators_for_this_platform(&shipped_codex_template("linux"));
    plant_codex_registry(home.path(), &template);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");
    assert_anchored_and_counted(&report, &sessions, "linux", &template);

    std::env::remove_var(scanner::REGISTRY_ENV);
}

/// The Windows shape with `CODEX_HOME` unset: `%USERPROFILE%\.codex\sessions\`,
/// whose documented default is the same `~/.codex/sessions` layer.
#[test]
fn windows_shape_without_codex_home_reaches_the_documented_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let sessions = home.path().join(".codex/sessions");
    plant_rollout(&sessions);
    let template = separators_for_this_platform(&shipped_codex_template("windows"));
    plant_codex_registry(home.path(), &template);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");
    assert_anchored_and_counted(&report, &sessions, "windows", &template);

    std::env::remove_var(scanner::REGISTRY_ENV);
}

/// And the override itself: an exported `CODEX_HOME` names the codex home, so
/// the sessions live at `<CODEX_HOME>/sessions` — not at the default, and not
/// nowhere. Run for both foreign cells, because the two spell the variable
/// differently and the report measured that *neither* honoured it.
#[test]
fn an_exported_codex_home_wins_over_the_documented_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for platform in ["linux", "windows"] {
        let home = tempfile::tempdir().unwrap();
        isolate_unconfigured_home(home.path());
        let codex_home = home.path().join("codex-home");
        let sessions = codex_home.join("sessions");
        plant_rollout(&sessions);
        // The default location exists too, and is empty: only the override can
        // produce the rollout that is counted below.
        fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);

        let template = separators_for_this_platform(&shipped_codex_template(platform));
        plant_codex_registry(home.path(), &template);

        let report = doctor::run();
        assert!(!report.scan_failed, "scratch registry must load");
        assert_anchored_and_counted(&report, &sessions, platform, &template);

        std::env::remove_var(scanner::REGISTRY_ENV);
        std::env::remove_var("CODEX_HOME");
    }
}

// ---------------------------------------------------------------------------
// §5 and §6 — what a user actually reads on the D3 line.
// ---------------------------------------------------------------------------

/// §6: the coverage header must not fold harnesses that were never looked at
/// into the number it reports as a measurement.
///
/// The old header was `{hit}/{known} known harnesses hit on this machine`, with
/// `known` every harness the registry lists for this platform — so on a machine
/// where some cells never resolved, the fraction read as "0 of 12 measured".
/// This asserts the three numbers now add up against the registry itself: the
/// denominator is the probed count, the never-probed remainder is printed
/// beside it, and the two together are the registry's harness count for this
/// platform. `aider` and `crush` keep a `$CWD` template on every platform, so
/// at least those two are always never-probed and the denominator is always
/// smaller than the registry — which is the whole point.
#[test]
fn the_coverage_header_counts_probed_and_never_probed_separately() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    std::env::remove_var(scanner::REGISTRY_ENV);

    let registry_len = scanner::load_registry_from_repo()
        .expect("shipped registry must load")
        .harnesses
        .len();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove(scanner::REGISTRY_ENV)
        .output()
        .expect("doctor must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "doctor must succeed; stderr={stderr}"
    );

    let header = stderr
        .lines()
        .find(|line| line.contains("probed harnesses hit on this machine"))
        .unwrap_or_else(|| panic!("the D3 coverage header must be printed; stderr={stderr}"));

    let fraction = header
        .split_whitespace()
        .find(|word| word.contains('/'))
        .expect("the header must carry a hit/probed fraction");
    let (hit, probed) = fraction
        .split_once('/')
        .expect("the fraction must be `hit/probed`");
    let (hit, probed): (usize, usize) = (
        hit.parse().expect("hit must be a number"),
        probed.parse().expect("probed must be a number"),
    );
    let never: usize = header
        .split(" probed harnesses hit")
        .nth(1)
        .and_then(|rest| rest.split(" not probed").next())
        .and_then(|rest| rest.rsplit('·').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!("the header must state how many were not probed; header={header}")
        });

    assert!(
        hit <= probed,
        "hit cannot exceed what was looked at: {header}"
    );
    assert!(
        never >= 2,
        "aider and crush have a `$CWD` template on every platform and are never probed, \
         so at least two harnesses must be named separately: {header}"
    );
    assert_eq!(
        probed + never,
        registry_len,
        "the denominator plus the never-probed remainder must be every registry harness, \
         not fewer and not more: {header}"
    );
    assert!(
        !stderr.contains("known harnesses hit on this machine"),
        "the old phrasing folded never-probed harnesses into a single measurement: {stderr}"
    );

    // The registry-driven table prints the same split, from the same numbers.
    let table = stderr
        .lines()
        .find(|line| line.contains("probed harnesses hit / scanned successfully"))
        .unwrap_or_else(|| panic!("the registry table must be printed; stderr={stderr}"));
    assert!(
        table.contains(&format!("{hit}/{probed}")),
        "the table and the header must agree on the same two numbers: header={header} table={table}"
    );
}

/// §5: the D3 timestamps are labelled with the clock they were read from.
///
/// A directory harness is probed from file metadata, so its row is mtime; a
/// single-file SQLite store carries the conversation's own time. Before this
/// they printed identically, and the risk line below called either one "your
/// earliest session" — which is how a session restored from a backup (fresh
/// mtime, old conversation) got described as "about 0 days ago".
#[test]
fn the_d3_session_line_names_the_clock_behind_its_timestamps() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_unconfigured_home(home.path());
    let projects = home.path().join(".claude/projects/p");
    fs::create_dir_all(&projects).unwrap();
    fs::write(projects.join("s.jsonl"), "{}\n").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove(scanner::REGISTRY_ENV)
        .output()
        .expect("doctor must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "doctor must succeed; stderr={stderr}"
    );

    let row = stderr
        .lines()
        .find(|line| line.trim_start().starts_with("claude-code") && line.contains("sessions"))
        .unwrap_or_else(|| {
            panic!("the claude-code footprint row must be printed; stderr={stderr}")
        });
    assert!(
        row.contains("earliest(mtime)"),
        "a directory harness's earliest time is a file mtime and must say so: {row}"
    );
    assert!(
        row.contains("latest(mtime)"),
        "same for the latest time: {row}"
    );
    assert!(
        !row.contains("· earliest 2"),
        "an unlabelled timestamp is the defect this replaces: {row}"
    );
}
