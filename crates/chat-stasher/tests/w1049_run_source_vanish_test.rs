//! W1049 (RUN-SOURCE-VANISH) — a source that vanishes mid-pass is resolved
//! by session identity, never by path.
//!
//! The round that motivated this (run-once 2026-10-27 20:45→22:17) scanned a
//! codex rollout under `sessions/`, and the store moved the file into
//! `archived_sessions/` during the collect window: the read at the scanned
//! path found nothing, the round reported `collect_incomplete` and pushed
//! nothing — while a path-hash search of both roots (the EXIT1-1027 probe)
//! could not even find the file, because a move necessarily changes the
//! path the hash was taken of. The approved fix searches by *identity* and
//! keeps the three states three (CLAUDE.md invariant 1):
//!
//! * `moved_away` — the same session id is confirmed inside a root this pass
//!   already collects. Collected there in this pass when the read succeeds
//!   (`MovedAwayNotice` records the ones a failed recovery read leaves), and
//!   never counted toward `collect_incomplete`.
//! * the unresolved vanish — identity in no collected root — keeps today's
//!   `CollectError`: the round stays incomplete and `run-once`'s gate
//!   semantics do not move (that question is a separate ticket).
//!
//! Every fixture here is a synthetic codex store built from the movable-tree
//! shape the shipped registry declares (W1007/DSK-1: primary `sessions/` plus
//! the `codex-archived` root, both roots pointed at sandbox directories
//! through config overrides — no environment is pinned, so the tests are
//! safe to run in parallel).

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[path = "../src/test_support.rs"]
mod test_support;
use test_support::Sandbox;

const MACHINE: &str = "fixture-machine";
/// The rollout filename is the session identity (stem), unchanged by the
/// store's own archive moves — the whole point of the movable root (W1007).
const STEM: &str = "rollout-2026-10-27T20-45-33-019bf00d-0000-7000-8000-00000000077b";

fn session_id() -> String {
    format!("codex.{MACHINE}.{STEM}")
}

/// The registry shape the shipped registry declares for codex (W1007/DSK-1):
/// a primary `sessions/` store plus the `codex-archived` movable root. A
/// fixture that wants a *non-movable* multi-root family (the gate test)
/// builds the same JSON without the `movable` flag.
fn codex_registry(movable_archived: bool) -> HarnessRegistry {
    let platform_key = scanner::current_platform();
    let cell = json!({
        "template": "~/.codex/sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1049 fixture"
    });
    let archived_cell = json!({
        "template": "~/.codex/archived_sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1049 fixture"
    });
    let paths = json!({ platform_key: cell });
    let archived_paths = json!({ platform_key: archived_cell });
    let mut archived_root = json!({
        "id": "codex-archived",
        "paths": archived_paths,
    });
    if movable_archived {
        archived_root["movable"] = json!(true);
        archived_root["provenance"] = json!({ "status": ["archived"] });
    }
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "W1049 synthetic",
        "harnesses": [{
            "id": "codex",
            "display_name": "synthetic codex",
            "paths": paths,
            "source_roots": [archived_root]
        }]
    }))
    .unwrap()
}

struct Fixture {
    _sandbox: Sandbox,
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
    active: PathBuf,
    archived: PathBuf,
    stage: PathBuf,
    state: PathBuf,
    registry: HarnessRegistry,
}

impl Fixture {
    fn new(movable_archived: bool) -> Self {
        let sandbox = Sandbox::new();
        let root = sandbox.root().to_path_buf();
        // Both roots are sandboxes themselves, pointed at through config
        // overrides — no `HOME` is rewritten, so no env lock is needed.
        let sessions_dir = root.join("codex-sessions");
        let archived_dir = root.join("codex-archived");
        // One segment per `join`: a multi-segment literal such as
        // `.join("2026/10/27")` is appended verbatim, so on Windows it
        // leaves `/` inside a path whose natural spelling is `\` — while
        // the scan's own walk (one `read_dir` entry at a time) writes the
        // native separator. The vanished-path digests below compare those
        // path *strings* byte for byte, so the fixture must name its
        // paths the same way the walk will name them on every platform.
        let active = sessions_dir
            .join("2026")
            .join("10")
            .join("27")
            .join(format!("{STEM}.jsonl"));
        let archived = archived_dir
            .join("2026")
            .join("10")
            .join("27")
            .join(format!("{STEM}.jsonl"));
        for path in [&active, &archived] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        let stage = root.join("stage");
        let state = root.join("state");
        fs::create_dir_all(&stage).unwrap();
        fs::create_dir_all(&state).unwrap();
        Fixture {
            _sandbox: sandbox,
            sessions_dir,
            archived_dir,
            active,
            archived,
            stage,
            state,
            registry: codex_registry(movable_archived),
        }
    }

    fn config(&self) -> Config {
        Config {
            codex_sessions_dir: Some(self.sessions_dir.to_string_lossy().into_owned()),
            harness_roots: BTreeMap::from([(
                "codex:codex-archived".to_string(),
                self.archived_dir.to_string_lossy().into_owned(),
            )]),
            ..Config::default()
        }
    }

    fn scan(&self) -> scanner::ScanReport {
        scanner::scan_with_registry_and_machine(&self.config(), &self.registry, MACHINE).unwrap()
    }

    fn collect(&self, scan: &scanner::ScanReport) -> collect::CollectReport {
        collect::collect_scan_report(
            scan,
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &DestinationView::unreachable("fixture-destination"),
        )
        .unwrap()
    }

    /// One collecting pass whose vanished sources re-walk the family's
    /// roots through the same probe shape `collect` wires in production:
    /// the registry narrowed to one entry, scanned by the ordinary
    /// machinery. `spy` records whether the vanish handling ever asked —
    /// and `None` makes the probe itself panic if called, which is how the
    /// never-probe assertions pin their classification happened first.
    fn collect_relocating(
        &self,
        scan: &scanner::ScanReport,
        spy: Option<&std::cell::Cell<bool>>,
    ) -> collect::CollectReport {
        let config = self.config();
        let registry = self.registry.clone();
        let relocate = move |harness_id: &str| -> anyhow::Result<scanner::ScanReport> {
            let Some(spy) = spy else {
                panic!("the relocation probe must not run for this pass: {harness_id}");
            };
            spy.set(true);
            let mut one = registry.clone();
            one.harnesses.retain(|entry| entry.id == harness_id);
            scanner::scan_with_registry_and_machine(&config, &one, MACHINE)
                .map_err(|error| anyhow::anyhow!("fixture rescan: {error}"))
        };
        collect::collect_scan_report_with_relocation(
            scan,
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &DestinationView::unreachable("fixture-destination"),
            Some(&relocate),
        )
        .unwrap()
    }

    /// The session's sealed body on the stage, by identity — the one number
    /// every "no duplicate session" assertion reduces to (W1007's helper,
    /// restated for this file).
    fn body(&self) -> Vec<u8> {
        store::concat_shards_in_dir(&store::session_shard_dir(
            &self.stage,
            MACHINE,
            &session_id(),
        ))
        .unwrap()
    }

    /// How many session directories the stage holds at all. A forked
    /// session is a second directory; this is the anti-fork assertion.
    fn staged_session_dirs(&self) -> Vec<String> {
        let dir = self.stage.join("sessions").join(MACHINE);
        let mut names: Vec<String> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn lines(turns: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    for turn in turns {
        body.extend_from_slice(format!(r#"{{"synthetic":"w1049-turn-{turn}"}}"#).as_bytes());
        body.push(b'\n');
    }
    body
}

/// The collector's own digest of a source path, computed the same way
/// `CollectError` and `MovedAwayNotice` do: canonicalized when possible
/// (the path as this process would name it), hashed as a string.
fn expect_path_digest(path: &std::path::Path) -> String {
    let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(key.to_string_lossy().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The ruling's required fixture, end to end: the store moves the rollout
/// into `archived_sessions/` **mid-collection** — here, between the two
/// root walks of one round's scan, so the round holds a sighting from each
/// root: the archived-root sighting (the file, at its new place) and the
/// sessions-root sighting (the vanished path). The round records
/// `moved_away` for the vanished sighting and stays collect-complete; the
/// **next** round receives the conversation that kept growing in its new
/// place, and produces no second session.
#[test]
fn mid_collection_archive_move_records_moved_away_and_next_round_receives() {
    let fx = Fixture::new(true);
    fs::write(&fx.active, lines(&[1, 2])).unwrap();

    // The round's scan, phase by phase: the primary-root walk saw the
    // rollout under sessions/ before the store moved it…
    let before_move = fx.scan();
    assert_eq!(before_move.records.len(), 1);
    // …then the store's thread/archive moved it, mid-collection…
    fs::rename(&fx.active, &fx.archived).unwrap();
    // …and the archived-root walk (the same round's later phase) sees the
    // same session identity at the new root. One scan, both sightings.
    let after_move = fx.scan();
    assert_eq!(after_move.records.len(), 1);
    assert_eq!(
        after_move.records[0].absolute_path, fx.archived,
        "the post-move walk finds the rollout at its new root"
    );
    let mut round_scan = after_move;
    round_scan.records.extend(before_move.records);
    assert_eq!(
        round_scan.records.len(),
        2,
        "the mid-collection move left one sighting per root"
    );

    let report = fx.collect(&round_scan);
    // The vanished sighting is not an error: the round's own sibling
    // collected the session at its new location.
    assert!(report.errors.is_empty(), "errors={:?}", report.errors);
    assert_eq!(report.moved_away.len(), 1, "one relocation notice");
    let notice = &report.moved_away[0];
    assert_eq!(
        notice.session_prefix,
        chat_stasher::id::short_session_id(&session_id()),
        "the notice names the session by prefix"
    );
    assert_eq!(
        notice.source_path_sha256,
        expect_path_digest(&fx.active),
        "the vanished path is recorded by digest"
    );
    assert_eq!(
        notice.relocated_path_sha256,
        expect_path_digest(&fx.archived),
        "the new location is recorded by digest"
    );
    // The archived-root sighting is the round's one outcome — bytes in,
    // exactly once; the vanished sighting sealed nothing and errored not.
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(
        report.outcomes[0].source_path_sha256, notice.relocated_path_sha256,
        "the round's outcome is the relocated sighting"
    );
    assert_eq!(fx.body(), lines(&[1, 2]));
    assert_eq!(fx.staged_session_dirs(), [session_id()]);

    // The conversation grows in its archived place — a next round that
    // received nothing would be a loss — and the next round receives it
    // without ever forking a second session.
    let mut grown = lines(&[1, 2]);
    grown.extend_from_slice(&lines(&[3]));
    fs::write(&fx.archived, &grown).unwrap();
    let report = fx.collect(&fx.scan());
    assert!(report.errors.is_empty(), "errors={:?}", report.errors);
    assert!(
        report.moved_away.is_empty(),
        "an ordinary pass records no relocation"
    );
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(
        fx.body(),
        grown,
        "the next round receives the appended turns"
    );
    assert_eq!(
        report.shards_written, 1,
        "the appended turn sealed as one shard"
    );
    assert_eq!(
        fx.staged_session_dirs(),
        [session_id()],
        "no duplicate session after the move"
    );
}

/// The cheap thing first (the ruling's must-fix 3): a vanish the *scan*
/// could not have seen — the file moved after the scan, before the
/// collector reached the record — is re-probed by identity before the pass
/// ends, and the session is collected **in this round** at its new
/// location: an ordinary outcome row, not even a `moved_away` trace.
#[test]
fn a_post_scan_move_is_collected_in_the_same_round_with_no_moved_away_trace() {
    let fx = Fixture::new(true);
    fs::write(&fx.active, lines(&[1, 2])).unwrap();

    // An ordinary first round establishes the cursor for the active path.
    let report = fx.collect(&fx.scan());
    assert!(report.errors.is_empty());
    assert_eq!(fx.body(), lines(&[1, 2]));

    // The next round's scan is taken with the file still active, so the
    // scan cannot contain the relocation — the store moved it afterwards,
    // mid-pass.
    let scan = fx.scan();
    assert_eq!(scan.records.len(), 1);
    fs::rename(&fx.active, &fx.archived).unwrap();

    let spy = std::cell::Cell::new(false);
    let report = fx.collect_relocating(&scan, Some(&spy));
    assert!(
        spy.get(),
        "the vanish handling must re-walk the family's roots"
    );
    assert!(report.errors.is_empty(), "errors={:?}", report.errors);
    assert!(
        report.moved_away.is_empty(),
        "a recovery that collected in-round leaves no moved_away trace: {:?}",
        report.moved_away
    );
    // The one outcome is the relocated sighting, collected this round: a
    // plain session outcome, counted exactly once.
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(
        report.outcomes[0].source_path_sha256,
        expect_path_digest(&fx.archived)
    );
    assert_eq!(
        report.changed_records, 0,
        "re-observed bytes are not new content"
    );
    assert_eq!(report.unchanged_records, 1);
    // W1007's content recovery: the bytes the archive already holds are
    // re-observed, never re-sealed — the body did not grow.
    assert_eq!(fx.body(), lines(&[1, 2]));
    assert_eq!(fx.staged_session_dirs(), [session_id()]);

    // An idempotent next pass over the moved file: still one session.
    let report = fx.collect(&fx.scan());
    assert!(report.errors.is_empty() && report.moved_away.is_empty());
    assert_eq!(fx.body(), lines(&[1, 2]));
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
}

/// The unresolved vanish keeps today's state exactly: identity found in no
/// root this pass collects is a collector error, the round stays
/// `collect_incomplete`, and the gate semantics do not move. The probe must
/// have actually looked before saying so.
#[test]
fn a_vanish_outside_every_collected_root_stays_today_s_error() {
    let fx = Fixture::new(true);
    fs::write(&fx.active, lines(&[1, 2])).unwrap();
    let scan = fx.scan();
    // The file is gone outright — not archived, not anywhere the registry's
    // roots cover.
    fs::remove_file(&fx.active).unwrap();

    let spy = std::cell::Cell::new(false);
    let report = fx.collect_relocating(&scan, Some(&spy));
    assert!(spy.get(), "the identity probe must run before giving up");
    assert!(report.moved_away.is_empty(), "no relocation was confirmed");
    assert_eq!(report.errors.len(), 1, "today's unknown keeps its error");
    assert_eq!(
        report.errors[0].session_prefix,
        chat_stasher::id::short_session_id(&session_id())
    );
    assert_eq!(
        report.errors[0].source_path_sha256,
        expect_path_digest(&fx.active),
        "the error still names the scanned path's digest"
    );
}

/// A read failure is not a vanish (CLAUDE.md invariant 1: "unreadable" and
/// "not there" are different states). A source whose path is still there
/// and will not read keeps today's error, and the identity probe never
/// runs — there is nothing to relocate.
#[test]
fn a_source_that_fails_to_read_is_not_a_vanish_and_never_probes() {
    let fx = Fixture::new(true);
    fs::write(&fx.active, lines(&[1, 2])).unwrap();
    let scan = fx.scan();
    // Replace the source with something present but unreadable as a file:
    // a directory at the exact path the scan enumerated.
    fs::remove_file(&fx.active).unwrap();
    fs::create_dir(&fx.active).unwrap();

    // The panicking probe is the assertion: a read failure must be
    // classified before any relocation is attempted.
    let report = fx.collect_relocating(&scan, None);
    assert_eq!(report.errors.len(), 1, "a read failure keeps its error");
    assert!(report.moved_away.is_empty());
    assert_eq!(fx.staged_session_dirs(), Vec::<String>::new());
}

/// Only a store the registry declares movable licenses the same-identity
/// rule. A family whose sibling root is not part of a movable store keeps
/// today's error for a vanished source even when a same-id file sits in the
/// other root — an aliased id is a slot merge (W1007), not proof the
/// vanished file moved, and the registry's `movable` flag is the only thing
/// that upgrades a same-id sighting into "the same file".
#[test]
fn a_non_movable_family_keeps_today_s_error_even_with_a_same_id_sibling() {
    let fx = Fixture::new(false);
    fs::write(&fx.active, lines(&[1, 2])).unwrap();
    // A same-id file in the sibling root of a NON-movable family.
    fs::write(&fx.archived, lines(&[1, 2])).unwrap();
    let scan = fx.scan();
    assert_eq!(scan.records.len(), 2, "both roots' sightings");
    fs::remove_file(&fx.active).unwrap();

    // The panicking probe asserts the gate: an unlicensed family is
    // classified before any identity search runs.
    let report = fx.collect_relocating(&scan, None);
    assert_eq!(
        report.errors.len(),
        1,
        "no movable root, no relocation license"
    );
    assert!(report.moved_away.is_empty());
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
}
