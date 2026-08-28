//! The "nothing is installed here" half of the doctor/registry consistency
//! invariant — the half a developer's own machine can never exercise, because
//! a developer's machine has every harness.
//!
//! `doctor_consistency_test.rs` plants real stores and checks the two tables
//! agree on the numbers. That only ever proves the *populated* case. The two
//! tables can also disagree about the meaning of **absence**, and that is the
//! failure CI hit: `footprint=Some(0) registry=None` for codex on
//! `ubuntu-latest`. `Some(0)` claims "I enumerated it, it is empty"; `None`
//! claims "I could not determine". Only one of those was earned.
//!
//! Both tests here run against an isolated temporary HOME and never look at,
//! walk, or depend on any real harness directory on the machine running them —
//! so they take the identical code path on a machine with every harness
//! installed and on a machine with none.

use chat_stasher::doctor::{self, HarnessFootprint};
use chat_stasher::scanner::{self, HarnessProbe};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// Every harness that appears in *both* doctor tables, as (footprint name,
/// registry probe id). Kept in the same shape as
/// `doctor_consistency_test::FOOTPRINT_TO_PROBE_ID` on purpose: the join is
/// explicit because gemini's two ids differ.
const FOOTPRINT_TO_PROBE_ID: [(&str, &str); 6] = [
    ("claude-code", "claude-code"),
    ("codex", "codex"),
    ("gemini", "gemini-cli"),
    ("opencode", "opencode"),
    ("cursor", "cursor"),
    ("grok", "grok"),
];

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point every base-directory variable this build consults at `home`, so no
/// probe can escape into the real machine.
fn isolate_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
    std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
    // Harness-specific overrides would re-point a root outside `home`.
    for var in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
    ] {
        std::env::remove_var(var);
    }
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

/// The invariant under test, stated as the "empty" question rather than the
/// "how many" question: a footprint row may only claim a number when the
/// registry probe actually enumerated the store. `Some(0)` opposite `None` is
/// a footprint asserting something nobody verified.
fn assert_absence_semantics_agree(report: &doctor::DoctorReport) {
    for (fp_name, probe_id) in FOOTPRINT_TO_PROBE_ID {
        let fp = footprint(report, fp_name);
        // A scratch registry may list only some harnesses; a row with no probe
        // at all is covered by `nothing_installed_reports_unknown_not_zero`.
        let Some(pr) = report.probes.iter().find(|p| p.id == probe_id) else {
            continue;
        };
        assert_eq!(
            fp.session_count.is_some(),
            pr.record_count.is_some(),
            "harness {fp_name} 对「空」的口径不一致: footprint={:?} registry={:?} \
             (probe state={:?}, installed_p={}) —— Some(0) 是「我看过，是空的」，None 是「我没能确定」",
            fp.session_count,
            pr.record_count,
            pr.state,
            pr.installed_p()
        );
        assert_eq!(
            fp.installed,
            pr.installed_p(),
            "harness {fp_name} 对「装没装」的口径不一致: footprint.installed={} registry.installed_p()={} \
             (probe state={:?})",
            fp.installed,
            pr.installed_p(),
            pr.state
        );
    }
}

/// A machine with **nothing installed**: an empty HOME and the shipped
/// registry. Every harness must resolve to "unknown", never to a count.
///
/// Note what this does *not* assert: it does not care whether a given cell
/// ends up `Missing`, `SkipUnascertained` or `SkipUnresolvable` — those differ
/// by platform. It asserts the invariant that holds on every platform, so the
/// test runs the same path on a fully-populated Mac and on a bare CI runner.
#[test]
fn nothing_installed_reports_unknown_not_zero() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_home(home.path());
    std::env::remove_var("CHAT_STASHER_REGISTRY");

    let report = doctor::run();
    assert!(!report.scan_failed, "shipped registry must load");

    assert_absence_semantics_agree(&report);

    for (fp_name, probe_id) in FOOTPRINT_TO_PROBE_ID {
        let fp = footprint(&report, fp_name);
        let pr = probe(&report, probe_id);
        assert!(
            !pr.installed_p(),
            "隔离 HOME 下 {probe_id} 不该被判为已安装（state={:?}）—— 测试泄漏到了本机真实目录",
            pr.state
        );
        assert_eq!(
            fp.session_count, None,
            "空机器上 {fp_name} 的 footprint 会话数必须是「未知」，不是 {:?}",
            fp.session_count
        );
        assert!(
            !fp.installed,
            "空机器上 {fp_name} 的 footprint 不该判为已安装"
        );
    }
}

/// The instrument's own reverse proof, and the exact CI reproduction.
///
/// A registry whose **every** platform cell for codex uses the unresolvable
/// `$CODEX_HOME/...` template — the shape the shipped registry ships for
/// `linux` — combined with a codex directory that *does* exist. All three
/// cells carry it on purpose: a registry that only fills in `linux` would make
/// this test silently stop testing on macOS, which is precisely how the
/// original bug reached CI green on macOS and red on Linux.
///
/// Before the fix this fails with the CI-shaped divergence
/// (`footprint=Some(0) registry=None`); the probe half of the assertions is
/// the proof that the instrument can still see a genuine "did not look".
#[test]
fn existing_dir_the_registry_never_resolved_is_unknown_not_zero() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    isolate_home(home.path());

    // A real codex session file, so `~/.codex/sessions` genuinely exists and
    // genuinely is not empty.
    let session = home
        .path()
        .join(".codex/sessions/2026-08-01/019bf00d-0000.jsonl");
    fs::create_dir_all(session.parent().unwrap()).unwrap();
    fs::write(&session, "{}\n").unwrap();

    let unresolvable = "$CODEX_HOME/sessions/（默认 $HOME/.codex/sessions/）";
    let registry_path = home.path().join("registry.json");
    let cell = format!(
        r#"{{ "template": "{unresolvable}", "env_override": "CODEX_HOME",
              "format": "jsonl / jsonl.zst", "confidence": "source-confirmed", "source": "B55 test" }}"#
    );
    fs::write(
        &registry_path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B55",
                  "harnesses": [
                    {{ "id": "codex", "display_name": "OpenAI Codex CLI",
                       "paths": {{ "macos": {cell}, "linux": {cell}, "windows": {cell} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var("CHAT_STASHER_REGISTRY", &registry_path);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    // Reverse proof: the probe really did fail to resolve a root, so there is
    // no "looked and found nothing" to report — `None` is the only honest cell.
    let pr = probe(&report, "codex");
    assert_eq!(
        pr.state,
        scanner::ProbeState::SkipUnresolvable,
        "前提失效：这条模板本应无法静态解析（note={}）",
        pr.note
    );
    assert_eq!(pr.record_count, None, "probe 侧必须是「未能确定」");

    // And the directory it was *about* is right there, non-empty — which is
    // exactly why printing 0 would be a lie rather than a harmless rounding.
    assert!(session.is_file(), "测试自身的前提：目录确实存在且非空");

    assert_absence_semantics_agree(&report);

    let fp = footprint(&report, "codex");
    assert_eq!(
        fp.session_count, None,
        "registry 从未解析出 codex 根路径，footprint 不得宣称会话数 {:?}",
        fp.session_count
    );

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}
