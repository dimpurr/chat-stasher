//! W1009 — DSK-3: doctor D3/D4 for the desktop roots.
//!
//! DSK-1 folded the Claude Desktop (Cowork) store into claude-code's
//! registry entry as a second source root and DSK-2 folded codex's
//! `archived_sessions/` into codex's; the aggregates already report the
//! W1006c/W1006d rules (measured-zero siblings, qualified `≥N` lower
//! bounds with the unverified roots named). These tests pin the doctor-side
//! halves of that wiring (45-DESKTOP-STORES §5 item 4, ADR-059 draft D5/D6):
//!
//!   * **D3** — one row per source root beside the family row: the primary
//!     store's own count, the Cowork root's own count, the active and the
//!     archived codex roots' own counts, with an unverified root keeping a
//!     marked row of its own instead of being folded into a number that
//!     reads as the family total.
//!   * **D4** — the registry-declared `desktopSessionCleanupPeriodDays`
//!     probe: missing → the built-in default 0 = no ceiling (≠ the 30-day
//!     CLI default), explicit 0, a positive ceiling whose effective
//!     retention is the longer of the two periods, and the managed
//!     `cleanupPeriodDays` override that makes the key ignored — named to
//!     the managing source, never read as "the user set X".
//!
//! Everything here is synthetic: a scratch registry, synthetic sessions
//! under a throwaway home, synthetic settings files. No real source is read
//! or written, and no run touches the machine's real configuration.

use chat_stasher::doctor;
use chat_stasher::scanner;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads;
/// every test in this file takes the lock for as long as it holds a
/// rewritten HOME and registry pointer.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Env {
    _lock: std::sync::MutexGuard<'static, ()>,
    /// Undo actions in reverse order of pinning, run on drop.
    restore: Vec<Box<dyn FnOnce()>>,
}

/// Pin HOME (and its Windows spelling), the XDG variables a config or state
/// read may resolve through, the registry pointer, and the per-harness
/// overrides — the same isolation `doctor_windows_shape_test` applies, held
/// by a guard so the pins cannot outlive the test (panic included).
fn pin_env(home: &Path, registry: &Path) -> Env {
    let lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut restore = Vec::new();

    for name in [
        "HOME",
        "USERPROFILE",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        let previous = std::env::var_os(name);
        std::env::set_var(name, home);
        restore.push(Box::new(move || match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }) as Box<dyn FnOnce()>);
    }
    for name in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
        "CLAUDE_CONFIG_DIR",
    ] {
        let previous = std::env::var_os(name);
        std::env::remove_var(name);
        restore.push(Box::new(move || match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }) as Box<dyn FnOnce()>);
    }

    let previous_registry = std::env::var_os(scanner::REGISTRY_ENV);
    std::env::set_var(scanner::REGISTRY_ENV, registry);
    restore.push(Box::new(move || match previous_registry {
        Some(value) => std::env::set_var(scanner::REGISTRY_ENV, value),
        None => std::env::remove_var(scanner::REGISTRY_ENV),
    }));

    Env {
        _lock: lock,
        restore,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for undo in self.restore.drain(..).rev() {
            undo();
        }
    }
}

/// The doctor CLI with the same pins applied to the child process, so a
/// stdout/stderr assertion runs against exactly the fixture the in-process
/// assertions ran against.
fn doctor_command(env: &Env, home: &Path, registry: &Path, json: bool) -> Command {
    let _guard = &env._lock;
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command.arg("doctor");
    if json {
        command.arg("--json");
    }
    for name in [
        "HOME",
        "USERPROFILE",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
    ] {
        command.env(name, home);
    }
    command.env(scanner::REGISTRY_ENV, registry);
    for name in ["CODEX_HOME", "GEMINI_CLI_HOME", "CLAUDE_CONFIG_DIR"] {
        command.env_remove(name);
    }
    command
}

/// The DSK-1 claude-code family shape (CLI primary + Cowork source root) and
/// the DSK-2 codex family shape (active primary + archived movable root), in
/// one scratch registry whose every platform slot carries the current cell
/// so the fixture runs the same on every OS.
fn desktop_roots_registry(dir: &Path) -> PathBuf {
    let platform = scanner::current_platform();
    let claude_cli = serde_json::json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "official-docs",
        "source": "synthetic W1009 fixture"
    });
    let cowork = serde_json::json!({
        "template": "~/Library/Application Support/Claude/local-agent-mode-sessions/",
        "format": "jsonl",
        "confidence": "measured-locally",
        "source": "synthetic W1009 fixture",
        "session_dir": {
            "pattern": "*/*/local_*",
            "file_glob": ".claude/projects/*/*.jsonl"
        }
    });
    let codex_active = serde_json::json!({
        "template": "~/.codex/sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1009 fixture"
    });
    let codex_archived = serde_json::json!({
        "template": "~/.codex/archived_sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1009 fixture"
    });
    let paths = serde_json::json!({
        "macos": claude_cli, "linux": claude_cli, "windows": claude_cli
    });
    let cowork_paths = serde_json::json!({
        "macos": cowork, "linux": cowork, "windows": cowork
    });
    let codex_paths = serde_json::json!({
        "macos": codex_active, "linux": codex_active, "windows": codex_active
    });
    let codex_archived_paths = serde_json::json!({
        "macos": codex_archived, "linux": codex_archived, "windows": codex_archived
    });
    let _ = platform;
    let registry = dir.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic W1009 fixture",
            "harnesses": [
                {
                    "id": "claude-code",
                    "display_name": "Claude Code",
                    "paths": paths,
                    "source_roots": [{
                        "id": "claude-desktop-cowork",
                        "paths": cowork_paths,
                        "provenance": { "surface": ["desktop-cowork"] }
                    }]
                },
                {
                    "id": "codex",
                    "display_name": "OpenAI Codex CLI",
                    "paths": codex_paths,
                    "source_roots": [{
                        "id": "codex-archived",
                        "paths": codex_archived_paths,
                        "movable": true,
                        "provenance": { "status": ["archived"] }
                    }]
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    registry
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// Plant 2 CLI transcripts, 1 Cowork sandbox transcript, and the codex
/// `thread/archive` leftover shape: one active rollout, one archived
/// rollout, and a leftover copy of the active rollout's uuid under the
/// archived root (W1007 — a copy is the same thread, not a second one).
fn plant_sessions(home: &Path) {
    for i in 0..2u8 {
        write(
            &home.join(format!(".claude/projects/repo-abc/019d5c10-000{i}.jsonl")),
            "{\"synthetic\":\"cli\"}\n",
        );
    }
    write(
        &home.join("Library/Application Support/Claude/local-agent-mode-sessions/space-1/acct-1/local_11111111-1111-1111-1111-111111111111/.claude/projects/desk-proj/019d5c10-cow1.jsonl"),
        "{\"synthetic\":\"cowork\"}\n",
    );
    write(
        &home.join(".codex/sessions/2026/09/01/rollout-2026-09-01T08-00-00-019d5c10-act1.jsonl"),
        "{\"synthetic\":\"codex-active\"}\n",
    );
    write(
        &home.join(
            ".codex/archived_sessions/2026/09/01/rollout-2026-09-01T09-00-00-019d5c10-arc1.jsonl",
        ),
        "{\"synthetic\":\"codex-archived\"}\n",
    );
    write(
        &home.join(
            ".codex/archived_sessions/2026/09/01/rollout-2026-09-01T08-00-00-019d5c10-act1.jsonl",
        ),
        "{\"synthetic\":\"codex-same-uuid\"}\n",
    );
}

/// D3 — one row per source root end to end, with the per-root scopes pinned
/// against the family aggregate: the claude-code family shows its CLI
/// primary and its Cowork root, the codex family its active and archived
/// roots, and the leftover-copy shape shows a per-root row counting the
/// copy in each root it appears in while the family counts the thread once
/// (`doctor_consistency_test`'s join stays intact either way).
#[test]
fn doctor_d3_prints_one_row_per_source_root_beside_the_family_row() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = desktop_roots_registry(dir.path());
    plant_sessions(&home);

    let env = pin_env(&home, &registry);
    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let claude_probe = report
        .probes
        .iter()
        .find(|p| p.id == "claude-code")
        .expect("claude-code probe");
    let claude_fp = report
        .footprints
        .iter()
        .find(|f| f.name == "claude-code")
        .expect("claude-code footprint");
    assert_eq!(
        claude_fp.session_count,
        Some(3),
        "2 CLI + 1 Cowork distinct ids in the family"
    );
    assert_eq!(
        claude_probe.source_roots.len(),
        2,
        "one row per declared root under the aggregate"
    );
    let (primary, cowork) = (&claude_probe.source_roots[0], &claude_probe.source_roots[1]);
    assert_eq!(
        (primary.id.as_str(), primary.primary, primary.record_count),
        ("claude-code", true, Some(2)),
        "the primary row is the CLI store's own count"
    );
    assert_eq!(
        (cowork.id.as_str(), cowork.primary, cowork.record_count),
        ("claude-desktop-cowork", false, Some(1)),
        "the Cowork row is the Cowork root's own count"
    );
    // The footprint table carries the same rows the registry probe table
    // carries — the two-table consistency the doctor join has always held,
    // extended to the per-root level.
    assert_eq!(
        claude_fp.source_roots.len(),
        claude_probe.source_roots.len()
    );
    assert_eq!(
        claude_fp.source_roots[1].id, cowork.id,
        "id for id between the two tables"
    );

    let codex_probe = report
        .probes
        .iter()
        .find(|p| p.id == "codex")
        .expect("codex probe");
    let codex_fp = report
        .footprints
        .iter()
        .find(|f| f.name == "codex")
        .expect("codex footprint");
    assert_eq!(
        codex_fp.session_count,
        Some(2),
        "the leftover copy of the active uuid is one thread, not two (W1007 join)"
    );
    let (codex_primary, codex_archived) =
        (&codex_probe.source_roots[0], &codex_probe.source_roots[1]);
    assert_eq!(codex_primary.id, "codex");
    assert_eq!(codex_primary.record_count, Some(1));
    assert_eq!(
        codex_archived.record_count,
        Some(2),
        "the archived row counts what its own root holds: the archived rollout \
         and the leftover copy — a per-root row's scope is the root, and the \
         family aggregate above is where the dedup happens"
    );

    // The human table prints the rows below the family lines, and a fully
    // measured family's rows carry plain numbers (no `≥`: the D6 marking is
    // the family aggregate's, printed there).
    let output = doctor_command(&env, &home, &registry, false)
        .output()
        .expect("run doctor");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("  claude-code sessions 3"),
        "the family row keeps its exact family count; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("primary                  sessions 2"),
        "the CLI primary row prints beside it; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("claude-desktop-cowork    sessions 1"),
        "the Cowork row prints beside it; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("codex-archived           sessions 2"),
        "the archived codex row prints with its own scope's count; stderr:\n{stderr}"
    );
    for row_line in stderr
        .lines()
        .filter(|line| line.starts_with("    ") && line.contains("sessions"))
    {
        assert!(
            !row_line.contains('≥'),
            "a measured root's own row is exact, never marked: {row_line:?}"
        );
    }

    // JSON: both tables carry the rows, through the same renderer.
    let json = doctor_command(&env, &home, &registry, true)
        .output()
        .expect("run doctor --json");
    let stdout = String::from_utf8_lossy(&json.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    for table in ["footprints", "probes"] {
        let rows = value[table]
            .as_array()
            .unwrap_or_else(|| panic!("{table} must be an array: {stdout}"))
            .iter()
            .find(|row| {
                row.get("name")
                    .map(|n| n == "claude-code")
                    .unwrap_or_else(|| row["harness"] == "claude-code")
            })
            .unwrap_or_else(|| panic!("no claude-code row in {table}: {stdout}"))["source_roots"]
            .as_array()
            .unwrap_or_else(|| panic!("source_roots must be an array in {table}: {stdout}"));
        assert_eq!(
            rows.len(),
            2,
            "one JSON root row per declared root in {table}"
        );
        assert_eq!(
            rows[1]["id"],
            serde_json::json!("claude-desktop-cowork"),
            "the desktop root is named by its registry id in {table}"
        );
        assert_eq!(
            rows[1]["session_count"],
            serde_json::json!({"kind": "known", "count": 1}),
            "the desktop root's own count, as a tri-state, in {table}"
        );
    }
    drop(env);
}

/// D3 — the unverified-root row: the shipped Windows shape (scannable CLI
/// primary beside an `unascertained` Cowork cell) replays on every OS, and
/// the unverified root keeps a **marked row of its own** — `unknown`, its
/// state word, and the explicit tie to the family's `≥` lower bound — so the
/// family number can never silently count as the whole family.
#[test]
fn an_unverified_source_root_keeps_its_own_marked_row_in_both_tables() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let primary = serde_json::json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "official-docs",
        "source": "synthetic W1009 fixture"
    });
    let unascertained = serde_json::json!({
        "template": "/nowhere/synthetic-guess/",
        "format": "jsonl",
        "confidence": "unascertained",
        "source": "synthetic W1009 fixture — the cell the scan refuses to guess at"
    });
    let registry_path = dir.path().join("registry.json");
    fs::write(
        &registry_path,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic W1009 fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "Claude Code",
                "paths": {
                    "macos": primary, "linux": primary, "windows": primary
                },
                "source_roots": [{
                    "id": "claude-desktop-cowork",
                    "paths": {
                        "macos": unascertained, "linux": unascertained,
                        "windows": unascertained
                    }
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    for i in 0..2u8 {
        write(
            &home.join(format!(".claude/projects/repo-abc/019d5c11-000{i}.jsonl")),
            "{\"synthetic\":\"cli\"}\n",
        );
    }

    let env = pin_env(&home, &registry_path);
    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "claude-code")
        .expect("claude-code probe");
    let footprint = report
        .footprints
        .iter()
        .find(|f| f.name == "claude-code")
        .expect("claude-code footprint");
    // The W1006d family aggregate, unchanged: ≥N over the measured roots,
    // the unverified root named.
    assert_eq!(probe.record_count, Some(2));
    assert_eq!(
        probe
            .unverified_roots
            .iter()
            .map(|root| root.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-desktop-cowork"]
    );
    assert_eq!(footprint.session_count, probe.record_count);
    assert_eq!(
        footprint.unverified_roots,
        vec!["claude-desktop-cowork".to_string()]
    );
    // The W1009 per-root rows: the unverified root has a row of its own,
    // marked, in both tables.
    let unverified_row = probe
        .source_roots
        .iter()
        .find(|row| row.id == "claude-desktop-cowork")
        .expect("the unverified root keeps a row");
    assert_eq!(unverified_row.state, scanner::ProbeState::SkipUnascertained);
    assert_eq!(
        unverified_row.record_count, None,
        "a root never looked at carries no count"
    );
    assert_eq!(
        footprint
            .source_roots
            .iter()
            .find(|row| row.id == "claude-desktop-cowork")
            .expect("the footprint carries the same row")
            .state,
        scanner::ProbeState::SkipUnascertained,
        "the footprint table carries the same marked row"
    );

    let output = doctor_command(&env, &home, &registry_path, false)
        .output()
        .expect("run doctor");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("  claude-code sessions ≥2"),
        "the family row keeps the qualified lower bound; stderr:\n{stderr}"
    );
    let marked = stderr
        .lines()
        .find(|line| line.contains("claude-desktop-cowork") && line.contains("unverified root"))
        .unwrap_or_else(|| {
            panic!("the unverified root's own marked row must print; stderr:\n{stderr}")
        });
    assert!(
        marked.contains("sessions unknown") && marked.contains("[skip_unascertained]"),
        "the marked row prints the uncounted count and its reason; got: {marked:?}"
    );

    let json = doctor_command(&env, &home, &registry_path, true)
        .output()
        .expect("run doctor --json");
    let stdout = String::from_utf8_lossy(&json.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    let row = &value["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "claude-code")
        .unwrap_or_else(|| panic!("no claude-code footprint row: {stdout}"))["source_roots"]
        .as_array()
        .unwrap()[1];
    assert_eq!(
        row["state"],
        serde_json::json!("skip_unascertained"),
        "the JSON row carries the state word; got: {row}"
    );
    assert_eq!(
        row["session_count"]["kind"],
        serde_json::json!("unknown"),
        "a root nobody looked at is unknown in JSON too, never a zero; got: {row}"
    );
    drop(env);
}

/// The registry fixture for the Desktop retention probe: the DSK-1 cell
/// shape (shipped macos cell's sources, declared for every platform so the
/// fixture runs everywhere), so the sources resolve inside the throwaway
/// home and never touch the machine's real settings.
fn retention_registry(dir: &Path) -> PathBuf {
    let cell = serde_json::json!({
        "user_sources": ["~/.claude/settings.json"],
        "managed_sources": ["~/.claude/managed-settings.json"],
        "confidence": "measured-locally",
        "source": "synthetic W1009 fixture"
    });
    let registry = dir.join("registry.json");
    let primary = serde_json::json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "official-docs",
        "source": "synthetic W1009 fixture"
    });
    fs::write(
        &registry,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic W1009 fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "Claude Code",
                "paths": {
                    "macos": primary, "linux": primary, "windows": primary
                },
                "retention_probes": [{
                    "id": "desktop-session-cleanup",
                    "key": "desktopSessionCleanupPeriodDays",
                    "states": {
                        "missing": "the key is not set: default 0, no ceiling",
                        "zero": "explicit 0: also no ceiling",
                        "positive_int": "a hard ceiling; effective is the longer period"
                    },
                    "managed_override": "a managed cleanupPeriodDays makes this key ignored",
                    "platforms": {
                        "macos": cell, "linux": cell, "windows": cell
                    }
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    registry
}

/// D4 — the three readable states of the desktop retention key, end to end
/// through `doctor::run`: missing → the built-in default 0 = no ceiling
/// (worded as itself, never as the 30-day CLI default), explicit 0, and a
/// positive ceiling whose effective retention is the longer of the two
/// periods. The D4 risk line rides the same report a human reads.
#[test]
fn doctor_d4_reports_the_three_desktop_retention_states() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = retention_registry(dir.path());

    // 1. Nothing set anywhere: the state is the default 0 = no ceiling, and
    //    the risk line says what that means for Desktop transcripts, in
    //    words that cannot be read as "30 days".
    let env = pin_env(&home, &registry);
    let report = doctor::run();
    let check = report
        .desktop_retention
        .as_ref()
        .expect("the fixture registry declares the desktop probe");
    assert!(
        matches!(check.state, doctor::DesktopRetention::DefaultZero),
        "a settings-free machine is in the default state, got {:?}",
        check.state
    );
    assert!(report
        .risks
        .iter()
        .any(|line| line.contains("default 0 = no ceiling") && line.contains("NOT the 30-day")));
    drop(env);

    // 2. An explicit 0 in the user settings: same retention, a different
    //    fact — the user chose it, and the state names their file.
    write(
        &home.join(".claude/settings.json"),
        r#"{"desktopSessionCleanupPeriodDays": 0}"#,
    );
    let env = pin_env(&home, &registry);
    let report = doctor::run();
    match &report
        .desktop_retention
        .as_ref()
        .expect("declared probe")
        .state
    {
        doctor::DesktopRetention::ExplicitZero { source } => {
            assert!(
                source.ends_with(".claude/settings.json"),
                "the state names the file that set the zero"
            );
        }
        other => panic!("expected ExplicitZero, got {other:?}"),
    }
    drop(env);

    // 3. A positive ceiling below a large CLI cleanup: the effective
    //    retention is the longer of the two periods (the CLI side wins
    //    here), and a paired red line rides D4 — the desktop transcripts
    //    were exempt before the key was set, which is exactly the risk a
    //    reader needs to see. The D1 half of the sentence names the
    //    fail-destructive caveat; this half names the desktop one.
    write(
        &home.join(".claude/settings.json"),
        r#"{"cleanupPeriodDays": 36500, "desktopSessionCleanupPeriodDays": 14}"#,
    );
    let env = pin_env(&home, &registry);
    let report = doctor::run();
    match &report
        .desktop_retention
        .as_ref()
        .expect("declared probe")
        .state
    {
        doctor::DesktopRetention::Ceiling {
            days,
            cleanup_days,
            effective_days,
            ..
        } => {
            assert_eq!(*days, 14);
            assert_eq!(*cleanup_days, Some(36500));
            assert_eq!(
                *effective_days,
                Some(36500),
                "the longer of the two periods, not the smaller"
            );
        }
        other => panic!("expected Ceiling, got {other:?}"),
    }
    assert!(report.risks.iter().any(|line| line.starts_with('🟡')
        && line.contains("desktopSessionCleanupPeriodDays = 14 days")
        && line.contains("hard age cap")));
    drop(env);
}

/// D4 — the managed override: a managed source that sets `cleanupPeriodDays`
/// makes the desktop key ignored entirely, and the report must name the
/// managed source, so "overridden by org policy" can never be read as
/// "the user set X" (DSK-0a, the retention-probe semantics section, item 3). A user desktop
/// value is present in this fixture on purpose — the override must win
/// over it.
#[test]
fn doctor_d4_reports_the_managed_override_with_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = retention_registry(dir.path());
    write(
        &home.join(".claude/settings.json"),
        r#"{"cleanupPeriodDays": 36500, "desktopSessionCleanupPeriodDays": 20000}"#,
    );
    write(
        &home.join(".claude/managed-settings.json"),
        r#"{"cleanupPeriodDays": 45}"#,
    );

    let env = pin_env(&home, &registry);
    let report = doctor::run();
    let check = report
        .desktop_retention
        .as_ref()
        .expect("the fixture registry declares the desktop probe");
    match &check.state {
        doctor::DesktopRetention::ManagedOverride { days, source } => {
            assert_eq!(*days, 45, "the policy value governs");
            assert!(
                source.ends_with(".claude/managed-settings.json"),
                "the report names the managing source, not the user's file"
            );
        }
        other => panic!("expected ManagedOverride, got {other:?}"),
    }
    assert!(report
        .risks
        .iter()
        .any(|line| line.contains("ignored") && line.contains("managed")));

    // And the same verdict in --json, tagged for scripts.
    let json = doctor_command(&env, &home, &registry, true)
        .output()
        .expect("run doctor --json");
    let stdout = String::from_utf8_lossy(&json.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    assert_eq!(
        value["desktop_retention"]["checked"],
        serde_json::json!(true)
    );
    assert_eq!(
        value["desktop_retention"]["state"]["kind"],
        serde_json::json!("managed_override")
    );
    assert_eq!(
        value["desktop_retention"]["state"]["days"],
        serde_json::json!(45)
    );
    drop(env);
}

/// The config-unreadable half: the desktop retention check reads settings
/// files the config never governs, so a broken config does not silence it —
/// the same standing D1/D2 have — while `not_checked` keeps naming only the
/// config-dependent checks (W1009/DSK-3).
#[test]
fn a_broken_config_still_reports_the_desktop_retention_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = retention_registry(dir.path());
    write(
        &home.join(".claude/settings.json"),
        r#"{"desktopSessionCleanupPeriodDays": 90}"#,
    );
    // `pin_env` points XDG_CONFIG_HOME at the home, so this is the config
    // path Config::load resolves first — broken TOML, the fail-closed shape.
    write(&home.join("chat-stasher/config.toml"), "[not valid toml");

    let env = pin_env(&home, &registry);
    let report = doctor::run();
    assert!(
        report.config_error.is_some(),
        "the fixture's config is the broken one"
    );
    assert!(
        report.scan_failed,
        "a broken config never reached the registry-driven scan"
    );
    let check = report
        .desktop_retention
        .as_ref()
        .expect("D1½ reads the machine's own settings files, not the config");
    match &check.state {
        doctor::DesktopRetention::Ceiling { days, .. } => assert_eq!(*days, 90),
        other => {
            panic!("expected the settings' ceiling to survive the broken config, got {other:?}")
        }
    }
    assert!(
        !report
            .not_checked()
            .iter()
            .any(|check| check.contains("Desktop") || check.contains("retention")),
        "a check that ran must not be listed as not-checked: {:?}",
        report.not_checked()
    );
    // The JSON shape keeps the two answers apart: D3 was not performed, the
    // desktop retention was.
    let json = doctor::report_to_json(&report);
    assert_eq!(
        json["desktop_retention"]["checked"],
        serde_json::json!(true)
    );
    assert_eq!(
        json["desktop_retention"]["state"]["kind"],
        serde_json::json!("ceiling")
    );
    drop(env);
}
