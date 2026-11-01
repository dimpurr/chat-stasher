//! CodeBuddy CLI source (W1047): the transcripts-only sibling of the
//! WorkBuddy dual store — the same `cbc` event core, walked as the flat
//! `projects/<id>.jsonl` and `projects/<slug>/<id>.jsonl` layouts, with no
//! sqlite half to join. These tests pin the W1047 arms of the shared
//! walker: a `*.jsonl` name the walk accepted but could not read is a
//! recognized session without a record — the transcripts-only counterpart
//! of the desktop store's live-row-without-transcript rule, a named gap
//! that turns `collect` partial (exit 3), never a zero; the `history`
//! input log is a declared machine log — measured at both levels of
//! projects, never a session, and with its body capture approved by
//! the owner's ruling (held by W1047f, approved 2026-10-31); and the
//! read side reuses the W1046 reader with no harness-specific branch.
//!
//! All fixtures are synthetic; no real conversation text, ids or names.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::normalize::{normalize, Block};
use chat_stasher::{scanner, store};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[path = "../src/test_support.rs"]
mod test_support;

/// Environment variables are process-global, and this suite reads one:
/// `HOME` decides where the declared machine log resolves, so the test that
/// pins it serializes on one lock the way the w860 suite does.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const MACHINE: &str = "synthetic-machine";

const USER_TURN: &str = r#"{"id":"synthetic-m1","timestamp":1780000000000,"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>synthetic injected context</system-reminder><user_query>synthetic genuine question</user_query>"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#;
const ASSISTANT_TURN: &str = r#"{"id":"synthetic-m2","timestamp":1780000000100,"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic answer"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#;
const SUMMARY_TURN: &str = r#"{"type":"summary","summary":"synthetic compaction summary","sessionId":"synthetic-session-id"}"#;
const TURN_METRICS_TURN: &str = r#"{"type":"turn-metrics","sessionId":"synthetic-session-id"}"#;

/// The synthetic registry this suite scans: a `codebuddy` entry whose
/// session cell says `~/.codebuddy/projects/` (the config states the
/// sandbox root the way a user's own `[harness_roots]` line would point at
/// a copied tree) and — where a test asserts it — the `history` machine-log
/// declaration the shipped registry carries.
fn synthetic_codebuddy_registry(with_machine_log: bool) -> scanner::HarnessRegistry {
    let platform = scanner::current_platform();
    let session_paths = json!({
        platform.clone(): {
            "template": "~/.codebuddy/projects/",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture",
        }
    });
    let machine_logs = if with_machine_log {
        json!([{
            "id": "history",
            "role": "machine-log",
            "paths": { platform.clone(): {
                "template": "~/.codebuddy/history.jsonl",
                "format": "jsonl",
                "confidence": "measured-locally",
                "source": "synthetic fixture",
            }},
            "notes": "synthetic fixture",
        }])
    } else {
        json!([])
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic fixture",
        "harnesses": [{
            "id": "codebuddy",
            "display_name": "CodeBuddy fixture",
            "paths": session_paths,
            "machine_logs": machine_logs,
        }]
    }))
    .unwrap()
}

/// Restore one environment variable on drop, including "was not set".
struct EnvReset(&'static str, Option<std::ffi::OsString>);

impl EnvReset {
    fn capture(name: &'static str) -> Self {
        EnvReset(name, std::env::var_os(name))
    }
}

impl Drop for EnvReset {
    fn drop(&mut self) {
        match self.1.take() {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

/// One CodeBuddy layout below `~/.codebuddy`: a `projects/` root that hosts
/// both the flat and the slug transcripts this source was measured with.
struct CodeBuddyFixture {
    sandbox: test_support::Sandbox,
}

impl CodeBuddyFixture {
    fn new() -> CodeBuddyFixture {
        CodeBuddyFixture {
            sandbox: test_support::Sandbox::new(),
        }
    }

    /// A flat-layout transcript: `projects/<session-id>.jsonl`.
    fn write_flat(&self, session_id: &str, body: &str) -> PathBuf {
        let path = self
            .sandbox
            .home()
            .join(format!(".codebuddy/projects/{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    /// A slug-layout transcript: `projects/<project-slug>/<session-id>.jsonl`.
    fn write_slug(&self, slug: &str, session_id: &str, body: &str) -> PathBuf {
        self.write_slug_file(slug, &format!("{session_id}.jsonl"), body)
    }

    /// A raw file inside a project directory, name exactly as given — the
    /// render caches and journals the core writes beside transcripts.
    fn write_slug_file(&self, slug: &str, file_name: &str, body: &str) -> PathBuf {
        let path = self
            .sandbox
            .home()
            .join(format!(".codebuddy/projects/{slug}/{file_name}"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    /// A file directly below the projects root, whatever its name.
    fn write_root_file(&self, name: &str, body: &str) -> PathBuf {
        let path = self
            .sandbox
            .home()
            .join(format!(".codebuddy/projects/{name}"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn scan(&self, with_machine_log: bool) -> scanner::ScanReport {
        let config = Config {
            harness_roots: [(
                "codebuddy".to_string(),
                self.sandbox
                    .home()
                    .join(".codebuddy/projects")
                    .to_string_lossy()
                    .into(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        scanner::scan_with_registry_and_machine(
            &config,
            &synthetic_codebuddy_registry(with_machine_log),
            MACHINE,
        )
        .unwrap()
    }

    fn collect(
        &self,
        scan: &scanner::ScanReport,
        stage: &std::path::Path,
    ) -> collect::CollectReport {
        let state = self.sandbox.root().join("state/collector.json");
        collect::collect_scan_report(
            scan,
            stage,
            MACHINE,
            &state,
            20,
            &DestinationView::unreachable("synthetic-destination"),
        )
        .unwrap()
    }

    fn collect_first(&self, scan: &scanner::ScanReport) -> (PathBuf, collect::CollectReport) {
        let stage = self.sandbox.root().join("stage");
        let report = self.collect(scan, &stage);
        (stage, report)
    }
}

fn body_string(stage: &std::path::Path, id: &str) -> String {
    let bytes = store::concat_shards(stage, MACHINE, id).unwrap();
    String::from_utf8(bytes).expect("codebuddy body is valid UTF-8")
}

fn session_id(native: &str) -> String {
    format!("codebuddy.{MACHINE}.{native}")
}

fn probe_of(scan: &scanner::ScanReport) -> &scanner::HarnessProbe {
    scan.probes.iter().find(|p| p.id == "codebuddy").unwrap()
}

#[test]
fn both_layouts_archive_one_id_each_and_the_count_is_a_measurement() {
    let fixture = CodeBuddyFixture::new();
    fixture.write_flat(
        "synthetic-session-001",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );
    fixture.write_slug(
        "synthetic-slug",
        "synthetic-session-002",
        &format!("{USER_TURN}\n"),
    );
    fixture.write_slug(
        "synthetic-slug",
        "synthetic-session-003",
        &format!("{USER_TURN}\n"),
    );

    let scan = fixture.scan(false);
    assert_eq!(scan.records.len(), 3, "one record per transcript id");
    for stem in [
        "synthetic-session-001",
        "synthetic-session-002",
        "synthetic-session-003",
    ] {
        assert!(
            scan.records
                .iter()
                .any(|record| record.id == session_id(stem)),
            "`{stem}` archives under its own file-name stem"
        );
    }
    let probe = probe_of(&scan);
    assert!(format!("{:?}", probe.state).contains("Scanned"));
    assert_eq!(probe.record_count, Some(3), "the walk is the whole store");
    assert_eq!(probe.unreadable_count, Some(0));
    assert_eq!(probe.unreadable_entry_count, Some(0));
    assert!(probe.note.contains("3 session file(s)"));
    assert!(probe.note.contains("flat=1"));
    assert!(probe.note.contains("stale duplicates skipped=0"));
    assert!(probe.note.contains("no sqlite session store beside them"));
    assert!(
        scan.archive_gaps().is_empty(),
        "everything recognized was archived"
    );
}

#[test]
fn an_empty_readable_root_is_a_measured_zero() {
    let fixture = CodeBuddyFixture::new();
    fs::create_dir_all(fixture.sandbox.home().join(".codebuddy/projects")).unwrap();

    let scan = fixture.scan(false);
    let probe = probe_of(&scan);
    assert_eq!(
        probe.record_count,
        Some(0),
        "a readable empty root is the one place a zero is earned"
    );
    assert!(probe.note.contains("0 session file(s)"));
    assert!(scan.archive_gaps().is_empty());
}

/// A `*.jsonl` name the walk accepted but could not read — the exit-3 gap
/// arm — cannot be produced through the filesystem on POSIX (stat succeeds
/// inside a readable directory whatever a file's own mode bits say), so it
/// is pinned by the scanner module's own unit test against the walk's
/// accounting directly; this suite keeps the arms the filesystem can make:
/// a chmod-000 file whose parent stays readable is still a session.
#[test]
#[cfg(unix)]
fn a_mode_zero_file_in_a_readable_directory_is_still_a_session() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CodeBuddyFixture::new();
    let quiet = fixture.write_flat("synthetic-session-010", "{}\n");
    fs::set_permissions(&quiet, fs::Permissions::from_mode(0o000)).unwrap();

    let scan = fixture.scan(false);
    let probe = probe_of(&scan);
    // POSIX honesty: stat needs the directory, not the file, so the walk
    // recognizes the file and hands it to collect; reading its bytes is
    // collect's failure to have, not a count to hide.
    assert_eq!(probe.record_count, Some(1));
    assert_eq!(probe.unreadable_count, Some(0));
    assert!(scan
        .records
        .iter()
        .any(|record| record.absolute_path == quiet));

    fs::set_permissions(&quiet, fs::Permissions::from_mode(0o644)).unwrap();
}

/// The same id in more than one location — the flat root and a project
/// directory — is one archive slot: the newest-mtime copy is archived and
/// the stale copy is named in the note.
#[test]
fn duplicate_copies_across_the_two_layouts_keep_the_newest_and_name_the_stale() {
    let fixture = CodeBuddyFixture::new();
    let stale = fixture.write_slug(
        "synthetic-old-slug",
        "synthetic-session-020",
        &format!("{USER_TURN}\n"),
    );
    let fresh = fixture.write_flat(
        "synthetic-session-020",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );
    // The flat copy is the newer one: backdate the slug copy past it.
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    let scan = fixture.scan(false);
    let transcripts: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == session_id("synthetic-session-020"))
        .collect();
    assert_eq!(transcripts.len(), 1, "one archive slot answers for one id");
    assert_eq!(transcripts[0].absolute_path, fresh);
    let probe = probe_of(&scan);
    assert_eq!(probe.record_count, Some(1));
    assert!(probe.note.contains("stale duplicates skipped=1"));
    assert!(scan.archive_gaps().is_empty());
}

/// What the walk is not: the input log's own name inside projects/ — flat or
/// under a project slug, both levels the CLI layout accepts — is excluded by
/// stem (the registry declares that file a machine log), the journals and
/// caches the core writes beside transcripts are excluded by glob and name,
/// and a subdirectory inside a project directory never holds transcripts.
#[test]
fn the_input_log_journals_caches_and_subdirectories_are_never_sessions() {
    let fixture = CodeBuddyFixture::new();
    fixture.write_flat("synthetic-session-030", &format!("{USER_TURN}\n"));

    let input_log_twin = fixture.write_root_file("history.jsonl", "{}\n");
    // The slug-level twin (W1047f Medium 3): the same stem one directory
    // deeper, the exact file the registry note's "inside projects/" covers
    // — its stem is never minted as the session id `history`.
    let slug_history_twin = fixture.write_slug_file("synthetic-slug", "history.jsonl", "{}\n");
    let rollback = fixture.write_root_file(
        "state.file-rollback.ndjson",
        r#"{"ca":"synthetic","commitSeq":1}"#,
    );
    // The slug layout's render cache: the file-history renderer's own
    // output, a `.txt` sidecar beside real transcripts — never a session.
    let render = fixture.write_slug_file(
        "synthetic-slug",
        "Read_7_synthetic-render.txt",
        "synthetic rendered bytes",
    );
    let tool_results = fixture
        .sandbox
        .home()
        .join(".codebuddy/projects/synthetic-slug/tool-results");
    fs::create_dir_all(&tool_results).unwrap();
    fs::write(tool_results.join("synthetic-session-031.jsonl"), "{}\n").unwrap();
    let nested = fixture
        .sandbox
        .home()
        .join(".codebuddy/projects/synthetic-slug/nested/synthetic-session-032.jsonl");
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    fs::write(&nested, "{}\n").unwrap();

    let scan = fixture.scan(false);
    for excluded in [
        &input_log_twin,
        &slug_history_twin,
        &rollback,
        &render,
        &nested,
    ] {
        assert!(
            scan.records
                .iter()
                .all(|record| record.absolute_path != *excluded),
            "{excluded:?} is not a session"
        );
    }
    assert!(
        !scan
            .records
            .iter()
            .any(|record| record.id.ends_with(".history") || record.id.contains(".history.")),
        "no `history` stem was ever minted into a session id, at either level"
    );
    assert!(
        scan.records
            .iter()
            .all(|record| !record.absolute_path.starts_with(&tool_results)),
        "the tool-results cache is excluded"
    );
    let probe = probe_of(&scan);
    assert_eq!(probe.record_count, Some(1));
    assert!(probe.note.contains("stale duplicates skipped=0"));
    assert!(scan.archive_gaps().is_empty());
}

/// The `history` machine log the registry entry declares: the scan half
/// measures it — presence, byte size, the `doctor` machine-log row — and
/// never lets it near a session count; the collect half is what the
/// W1047f gate holds, and the owner's ruling (W1047f2) approved the
/// capture, so the flag is `true` and the ruling-approved branch is the
/// branch this build takes. Both branches of the flag assert, so neither
/// state of the one-flag reversal can quietly strand the pin: sealing
/// withheld means measured-and-sealed-nothing (never a log found
/// empty), and the ruling-approved state is the full ADR-053 capture the
/// claude-code log already gets.
#[test]
fn the_input_log_is_measured_never_a_session_and_seal_gated_by_the_ruling() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = CodeBuddyFixture::new();
    // The machine-log cell resolves through `~`, which in a test process
    // points at the machine's real home until this redirect lands; the
    // restore guard keeps the change test-local.
    let _home = EnvReset::capture("HOME");
    std::env::set_var("HOME", fixture.sandbox.home());
    let history_line = |display: &str, at_ms: u64| {
        format!(
            "{}\n",
            json!({
                "display": display,
                "pastedContents": {},
                "project": "/synthetic/project",
                "timestamp": at_ms,
            })
        )
    };
    let history = [history_line("/first", 1), history_line("/second", 2)].concat();
    let log_path = fixture.sandbox.home().join(".codebuddy/history.jsonl");
    fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    fs::write(&log_path, &history).unwrap();
    fixture.write_slug(
        "synthetic-slug",
        "synthetic-session-040",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );

    let scan = fixture.scan(true);
    assert_eq!(scan.machine_logs.len(), 1, "the declared log exists");
    assert_eq!(scan.machine_logs[0].harness, "codebuddy");
    assert_eq!(scan.machine_logs[0].log_id, "history");
    assert_eq!(
        scan.machine_logs[0].absolute_path, log_path,
        "the log resolved through the sandbox home, not the real one"
    );
    assert_eq!(
        scan.machine_logs[0].byte_size,
        history.len() as u64,
        "presence and bytes are measured whatever the sealing gate does"
    );

    let (stage, report) = fixture.collect_first(&scan);
    assert_eq!(report.machine_logs.len(), 1);
    let outcome = &report.machine_logs[0];
    assert_eq!(outcome.harness, "codebuddy");
    assert_eq!(outcome.log_id, "history");
    assert_eq!(report.changed_records, 1, "one session record");
    let body = body_string(&stage, &session_id("synthetic-session-040"));
    assert!(body.contains("synthetic genuine question"));
    assert!(
        !body.contains("/first"),
        "the input log never reaches a session body"
    );

    let log_dir = store::machine_log_shard_dir(&stage, MACHINE, "codebuddy", "history");
    if collect::CODEBUDDY_HISTORY_BODY_CAPTURE {
        // The ruling-approved state: full ADR-053 capture, the W1047 shape.
        assert!(!outcome.content_withheld);
        assert_eq!(outcome.lines_written, 2);
        assert_eq!(outcome.generations, 1);
        assert!(
            outcome.sealed_lines > 0,
            "the ruling-approved flip seals the log's lines, not zeros"
        );
        let sealed = store::concat_shards_in_dir(&log_dir).unwrap();
        assert_eq!(String::from_utf8(sealed).unwrap(), history);
        // A quiet pass: neither half has new bytes, so both count zero.
        let scan = fixture.scan(true);
        let report = fixture.collect(&scan, &stage);
        assert_eq!(report.machine_logs[0].lines_written, 0);
        assert_eq!(report.machine_logs[0].generations, 1);
        assert_eq!(report.changed_records, 0);
    } else {
        // The withheld state (W1047f High 1): measured, sealed nothing —
        // a counted fact beside honest zeros, never a log found empty and
        // never an obstacle to the session half.
        assert!(
            outcome.content_withheld,
            "the one log whose body capture is withheld says so in the outcome"
        );
        assert_eq!(outcome.lines_written, 0);
        assert_eq!(outcome.generations, 0);
        assert_eq!(outcome.shard, None);
        assert_eq!(
            outcome.source_bytes,
            history.len() as u64,
            "the withheld outcome reports the scan's measurement"
        );
        let sealed_any = match fs::read_dir(&log_dir) {
            Ok(entries) => entries.count() > 0,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("read the machine-log namespace: {error}"),
        };
        assert!(
            !sealed_any,
            "nothing from the input log's content was sealed anywhere"
        );
        // A second pass is identical: no cursor was written, so the
        // withheld log neither accretes state nor changes its story.
        let scan = fixture.scan(true);
        let report = fixture.collect(&scan, &stage);
        assert!(
            report.machine_logs[0].content_withheld,
            "a second withheld pass reads identically"
        );
        assert_eq!(report.machine_logs[0].lines_written, 0);
        assert_eq!(report.machine_logs[0].generations, 0);
        assert_eq!(report.changed_records, 0);
        assert_eq!(
            body_string(&stage, &session_id("synthetic-session-040")),
            body
        );
    }
}

/// The read side: a collected body renders through the W1046 reader with
/// no codebuddy-specific branch, and the same body renders identically
/// under the `workbuddy` id — the same-core reuse, pinned.
#[test]
fn the_read_reuses_the_workbuddy_reader_with_no_codebuddy_branch() {
    let fixture = CodeBuddyFixture::new();
    fixture.write_flat(
        "synthetic-session-050",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n{SUMMARY_TURN}\n{TURN_METRICS_TURN}\n"),
    );

    let scan = fixture.scan(false);
    let (stage, _) = fixture.collect_first(&scan);
    let id = session_id("synthetic-session-050");
    let body = body_string(&stage, &id);
    assert_eq!(body.lines().count(), 4, "no metadata line was joined in");

    let rendered_texts = |harness: &str| {
        let conversation = normalize(harness, &body);
        conversation
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<String>>()
    };
    assert_eq!(
        rendered_texts("codebuddy"),
        vec!["synthetic genuine question", "synthetic answer"],
        "the CLI-only summary and turn-metrics lines count as bookkeeping"
    );
    assert_eq!(
        rendered_texts("workbuddy"),
        rendered_texts("codebuddy"),
        "one reader, two registry harnesses — the bodies render identically"
    );
    let conversation = normalize("codebuddy", &body);
    assert!(!format!("{conversation:?}").contains("synthetic injected context"));

    // A quiet pass stages nothing.
    let report = fixture.collect(&scan, &stage);
    assert_eq!(report.changed_records, 0);
    assert_eq!(body_string(&stage, &id), body);
}

/// A projects root the walker could not list at all: unknown, not zero,
/// and never a claimed count. The collect half (W1047f Medium 5) pins the
/// chain the cross-family review described: a `record_count` of `None`
/// never becomes an archive gap (`archive_gaps` maps the None count to
/// prose), so the pass neither fails nor reports PARTIAL — the printed
/// report's `scan partial` line is where the entry-level unknown lives
/// (the unreadable-entry tally is what the report narrates), and the exit
/// the ladder answers is 0. The exit ladder itself is pinned directly in
/// the bin's own tests beside `cmd_collect`.
#[test]
#[cfg(unix)]
fn an_unlistable_root_keeps_the_count_unknown_never_zero() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CodeBuddyFixture::new();
    let root = fixture.sandbox.home().join(".codebuddy/projects");
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

    let scan = fixture.scan(false);
    let probe = probe_of(&scan);
    assert_eq!(
        probe.record_count, None,
        "nothing was recognized, so no count is a measurement"
    );
    assert!(
        probe.note.contains("unknown, not 0"),
        "the note says unknown rather than claiming emptiness"
    );
    assert_eq!(probe.unreadable_entry_count, Some(1));

    let (_, report) = fixture.collect_first(&scan);
    assert!(
        report.archive_gaps.is_empty(),
        "an entry-level unknown is prose, not an archive gap"
    );
    assert!(report.errors.is_empty());
    assert!(report.codebuff_torn_skips.is_empty());
    assert_eq!(
        report.scanner_unreadable_entry_count, 1,
        "the printed report's scan-partial line carries exactly this tally"
    );

    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A partial walk: one readable transcript archives while one slug
/// directory cannot be listed, and the entry-level tally rides along
/// without turning the whole store into an unknown.
#[test]
#[cfg(unix)]
fn a_partial_walk_archives_the_readable_half_and_counts_the_entry() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CodeBuddyFixture::new();
    fixture.write_flat("synthetic-session-060", &format!("{USER_TURN}\n"));
    let unlistable = fixture
        .sandbox
        .home()
        .join(".codebuddy/projects/synthetic-unlistable");
    fs::create_dir_all(&unlistable).unwrap();
    fs::set_permissions(&unlistable, fs::Permissions::from_mode(0o000)).unwrap();

    let scan = fixture.scan(false);
    let probe = probe_of(&scan);
    assert_eq!(probe.record_count, Some(1));
    assert_eq!(probe.unreadable_entry_count, Some(1));
    assert!(probe
        .note
        .contains("transcript walk incomplete: 1 unreadable directory entries"));
    assert_eq!(scan.records.len(), 1);
    assert!(scan.archive_gaps().is_empty());

    fs::set_permissions(&unlistable, fs::Permissions::from_mode(0o755)).unwrap();
}
