//! W902 · ADR-055 D4 raw export archiving.
//!
//! Every byte here is a synthetic canary planted in a sandboxed stage: a real
//! user takeout is never read by these tests, and nothing here writes outside
//! [`test_support::Sandbox`].
//!
//! The property under test is the *isolation*, not the sealing: a raw export
//! file must reach the archive byte-exact and change no session count anywhere,
//! because "N sessions archived" is the number a reader trusts when they decide
//! whether their conversations are covered. Counting the package would
//! overclaim coverage, which is why ADR-055 lists it as a rejected alternative.

use chat_stasher::manifest;
use chat_stasher::message_audit::Fidelity;
use chat_stasher::metahash;
use chat_stasher::raw_export::{self, ExportIndexState, SealOutcome};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "fixture-machine";
const DEEPSEEK_NAME: &str = "deepseek-export.bin";
const CHATGPT_NAME: &str = "chatgpt-dsar.zip";
const DEEPSEEK_CANARY: &[u8] =
    b"PK\x03\x04\x14\x00\x00\x00\x00\x00 chat-stasher W902 synthetic takeout canary: not a real export, no conversation text.\n";
const DEEPSEEK_SHA: &str = "2d5b825b1b182de959f33f5dddc31ee2196e91eb3b6801650c38d2145cad23a9";
const CHATGPT_CANARY: &[u8] =
    b"PK\x03\x04\x14\x00\x00\x00\x00\x00 chat-stasher W902 synthetic takeout canary, second distinct package.\n";
const CHATGPT_SHA: &str = "cda2b4cfbec609a358f127f44c5fd60614cd05157ab48aa7ce27cec1d429c6ba";
const SESSIONS: [&str; 2] = ["synthetic.session-one", "synthetic.session-two"];

fn archive(sandbox_root: &Path) -> (BackupStore, MasterKey) {
    let cfg = StoreConfig {
        repo_root: sandbox_root.join("archive").to_string_lossy().into_owned(),
        key_file: sandbox_root.join("archive-key.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(test_support::rustic_cache_root(sandbox_root)),
        no_cache: false,
    };
    let key = MasterKey::new();
    store::persist_key_file(&cfg, &key).expect("persist the fixture key");
    (BackupStore::new(cfg, MACHINE.to_string()), key)
}

/// Two sessions of real shard content, so the numbers a count would wrongly
/// absorb actually exist to be compared.
fn seed_sessions(stage: &Path) {
    for (index, session_id) in SESSIONS.iter().enumerate() {
        for shard in 0..2 {
            store::write_sealed_shard_bytes_with_cap(
                StageWriter::Collect,
                stage,
                MACHINE,
                session_id,
                &[format!(r#"{{"session":"{session_id}","row":{index}{shard}}}"#).into_bytes()],
                store::DEFAULT_SHARD_BUCKET_CAP,
            )
            .expect("seed a fixture shard");
        }
    }
}

/// Every number a session count is made of, rendered for byte comparison.
fn session_ledger(stage: &Path) -> String {
    let mut lines = vec![format!(
        "sealed_shards={}",
        store::sealed_shard_count(stage).expect("count sealed shards")
    )];
    for row in manifest::generate_manifest_at(stage, MACHINE, 0).expect("derive the manifest") {
        lines.push(format!(
            "{}|{}|{}|{}",
            row.session_id, row.shard_count, row.concat_bytes, row.concat_sha256
        ));
    }
    lines.sort();
    lines.join("\n")
}

/// `(platform, content address, delivered name)` of every object in the
/// namespace, read straight off the disk rather than through the module's own
/// matcher.
fn archived_objects(stage: &Path) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for platform_entry in fs::read_dir(raw_export::export_files_root(stage))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
    {
        let platform = platform_entry.file_name().to_string_lossy().into_owned();
        for key_entry in fs::read_dir(platform_entry.path())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
        {
            let key = key_entry.file_name().to_string_lossy().into_owned();
            for object in fs::read_dir(key_entry.path())
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
            {
                let name = object.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.starts_with('.'),
                    "an interrupted copy must never sit in the namespace as the object: {name}"
                );
                assert!(
                    object.file_type().expect("object entry type").is_file(),
                    "a content address holds objects, not directories: {name}"
                );
                out.push((platform.clone(), key.clone(), name));
            }
        }
    }
    out.sort();
    out
}

/// Plant a synthetic takeout in the sandbox's download directory, creating any
/// parent the caller names. The path says where the file was found, never where
/// it is archived.
fn source(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join("Downloads").join(name);
    fs::create_dir_all(path.parent().expect("a parent directory"))
        .expect("the fixture download directory");
    fs::write(&path, bytes).expect("write the fixture takeout");
    path
}

#[test]
fn one_raw_export_seals_once_byte_exact_and_a_repeat_is_a_no_op() {
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).expect("fixture stage");
    let first = source(sandbox.root(), DEEPSEEK_NAME, DEEPSEEK_CANARY);

    let outcome = raw_export::seal_export_file(&stage, MACHINE, "deepseek", &first)
        .expect("seal the fixture export");
    let SealOutcome::Installed {
        content_sha256,
        bytes,
        delivered_name,
        path,
    } = &outcome
    else {
        panic!("the first copy of a package is installed, not skipped: {outcome:?}");
    };
    assert_eq!(content_sha256, DEEPSEEK_SHA);
    assert_eq!(*bytes, DEEPSEEK_CANARY.len() as u64);
    assert_eq!(delivered_name, DEEPSEEK_NAME);
    assert_eq!(
        path,
        &raw_export::export_object_file(&stage, "deepseek", DEEPSEEK_SHA, DEEPSEEK_NAME),
        "the object sits at D4's key, under the name the file was delivered with"
    );
    assert_eq!(
        path,
        &stage
            .join("export-files")
            .join("deepseek")
            .join(DEEPSEEK_SHA)
            .join(DEEPSEEK_NAME)
    );
    assert_eq!(fs::read(path).expect("read the object"), DEEPSEEK_CANARY);
    assert_eq!(
        fs::read(&first).expect("the source is still there"),
        DEEPSEEK_CANARY,
        "archiving copies: it never moves or rewrites what the user downloaded"
    );

    // The same package in a second place under a second name (ADR-055 F7: it
    // usually lives in two or three) is recognised before any byte is copied,
    // and the name it keeps is the one of the copy that got there first.
    let repeat = source(
        sandbox.root(),
        "backup/deepseek-export-copy.bin",
        DEEPSEEK_CANARY,
    );
    let again = raw_export::seal_export_file(&stage, MACHINE, "deepseek", &repeat)
        .expect("seal the identical package again");
    assert_eq!(
        again,
        SealOutcome::AlreadyArchived {
            content_sha256: DEEPSEEK_SHA.to_string(),
            bytes: DEEPSEEK_CANARY.len() as u64,
            held_name: DEEPSEEK_NAME.to_string(),
        },
        "a byte-identical repeat must not store a second copy"
    );
    assert_eq!(
        archived_objects(&stage),
        vec![(
            "deepseek".to_string(),
            DEEPSEEK_SHA.to_string(),
            DEEPSEEK_NAME.to_string()
        )]
    );

    // A distinct export is always kept.
    let distinct = source(sandbox.root(), CHATGPT_NAME, CHATGPT_CANARY);
    let outcome = raw_export::seal_export_file(&stage, MACHINE, "chatgpt", &distinct)
        .expect("seal the second package");
    assert!(
        matches!(outcome, SealOutcome::Installed { .. }),
        "a different file is a different object: {outcome:?}"
    );
    assert_eq!(
        archived_objects(&stage),
        vec![
            (
                "chatgpt".to_string(),
                CHATGPT_SHA.to_string(),
                CHATGPT_NAME.to_string()
            ),
            (
                "deepseek".to_string(),
                DEEPSEEK_SHA.to_string(),
                DEEPSEEK_NAME.to_string()
            ),
        ]
    );

    let ExportIndexState::Loaded(rows) = raw_export::read_export_index(&stage, MACHINE).unwrap()
    else {
        panic!("two archived objects must leave two index rows");
    };
    assert_eq!(rows.len(), 2);
    let expected = [
        ("chatgpt", CHATGPT_SHA, CHATGPT_CANARY.len() as u64),
        ("deepseek", DEEPSEEK_SHA, DEEPSEEK_CANARY.len() as u64),
    ];
    for row in &rows {
        assert_eq!(row.machine, MACHINE);
        assert_eq!(
            row.fidelity.fidelity,
            Fidelity::Raw,
            "a raw export's fidelity stamp is `raw`"
        );
        let (_, content_sha256, bytes) = expected
            .iter()
            .find(|(name, key, _)| *name == row.platform && *key == row.content_sha256)
            .unwrap_or_else(|| panic!("an unexpected index row: {row:?}"));
        assert_eq!(&row.content_sha256, content_sha256);
        assert_eq!(row.bytes, *bytes, "the row records the bytes it stands for");
        // The row is metadata about the bytes: no path and no file name of the
        // place the file was found. The schema id legitimately carries a `/`, so
        // the test is the *paths themselves* plus the names, not any separator.
        let rendered = serde_json::to_string(row).unwrap();
        for absent in [
            DEEPSEEK_NAME,
            CHATGPT_NAME,
            "deepseek-export-copy.bin",
            "Downloads",
            &sandbox.root().to_string_lossy(),
            &stage.to_string_lossy(),
        ] {
            assert!(
                !rendered.contains(absent),
                "an index row carries digests, not paths: {rendered}"
            );
        }
    }
}

#[test]
fn an_object_whose_bytes_are_not_its_name_is_not_treated_as_a_repeat() {
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).expect("fixture stage");
    let export = source(sandbox.root(), DEEPSEEK_NAME, DEEPSEEK_CANARY);
    raw_export::seal_export_file(&stage, MACHINE, "deepseek", &export).expect("seal");

    // Something wrote a different-length body under a content address it does not
    // match: the archive holds bytes that are not the ones its name claims, which
    // is not the "already archived" answer.
    let object = raw_export::export_object_file(&stage, "deepseek", DEEPSEEK_SHA, DEEPSEEK_NAME);
    fs::write(&object, b"tampered with").expect("tamper with the object");
    let error = raw_export::seal_export_file(&stage, MACHINE, "deepseek", &export)
        .expect_err("a body that is not its address must not pass as the archive's copy");
    assert!(
        error.to_string().contains("not the bytes its name claims"),
        "{error}"
    );
}

#[test]
fn an_export_object_changes_no_session_count_and_reaches_the_archive() {
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).expect("fixture stage");
    seed_sessions(&stage);
    let (archive, key) = archive(sandbox.root());

    let before = session_ledger(&stage);
    assert_eq!(before.lines().count(), 3, "4 shards across 2 sessions");
    let meta_before = metahash::compute_meta_hash(&stage, MACHINE).expect("meta hash");
    let baseline = archive.push(&stage, &key).expect("push the sessions alone");
    assert_eq!(baseline.stage_shards, 4);

    let export = source(sandbox.root(), DEEPSEEK_NAME, DEEPSEEK_CANARY);
    raw_export::seal_export_file(&stage, MACHINE, "deepseek", &export).expect("seal");

    // The isolation property, stated as bytes rather than as an intention.
    assert_eq!(
        session_ledger(&stage),
        before,
        "an archived export file must not move any session count"
    );

    // And the object is nonetheless part of the run: the index row rides the
    // metadata digest, so a stage whose only change is an export pushes.
    let meta_after = metahash::compute_meta_hash(&stage, MACHINE).expect("meta hash");
    assert!(
        meta_before != meta_after,
        "the export object must be bound into the metadata digest, got {meta_before:?}"
    );
    let summary = archive
        .push(&stage, &key)
        .expect("push with the export object");
    assert_eq!(
        summary.stage_shards, 4,
        "push reports the session shards it sealed, not the export object"
    );

    let archived = raw_export::read_archived_export(
        &archive,
        &key,
        MACHINE,
        "deepseek",
        DEEPSEEK_SHA,
        &mut io::sink(),
    )
    .expect("read the export object back");
    assert_eq!(archived.content_sha256, DEEPSEEK_SHA);
    assert_eq!(archived.bytes, DEEPSEEK_CANARY.len() as u64);
    assert_eq!(
        archived.delivered_name, DEEPSEEK_NAME,
        "the archive hands the object back under the name it was delivered with"
    );

    let into_a_file = sandbox.root().join("restored").join(DEEPSEEK_NAME);
    fs::create_dir_all(into_a_file.parent().unwrap()).expect("restore directory");
    {
        let mut file = fs::File::create(&into_a_file).expect("create the restore file");
        let archived = raw_export::read_archived_export(
            &archive,
            &key,
            MACHINE,
            "deepseek",
            DEEPSEEK_SHA,
            &mut file,
        )
        .expect("read the export object into a file");
        assert_eq!(archived.content_sha256, DEEPSEEK_SHA);
        assert_eq!(archived.bytes, DEEPSEEK_CANARY.len() as u64);
        file.flush().expect("flush the restored bytes");
    }
    assert_eq!(
        fs::read(&into_a_file).expect("read the restored file"),
        DEEPSEEK_CANARY,
        "what the archive hands back is what the platform produced"
    );

    // A session read still reads sessions only.
    for session_id in SESSIONS {
        let (bytes, shards) = archive
            .read_session_concat(MACHINE, session_id, &key)
            .expect("read a fixture session back");
        assert_eq!(
            shards.len(),
            2,
            "{session_id} holds its two shards and no more"
        );
        assert_eq!(
            bytes,
            store::concat_shards(&stage, MACHINE, session_id).unwrap()
        );
    }

    // An object the archive does not hold is an error about the reads that
    // happened, never an empty success.
    let never_sealed = "0".repeat(64);
    let error = raw_export::read_archived_export(
        &archive,
        &key,
        MACHINE,
        "deepseek",
        &never_sealed,
        &mut io::sink(),
    )
    .expect_err("asking for an unarchived object must fail");
    assert!(
        error.to_string().contains("all of them were read"),
        "{error}"
    );
}

#[test]
fn a_stage_holding_only_an_export_object_still_reaches_the_archive() {
    // The empty-snapshot guard counts session shards. If the export namespace
    // were invisible to it, an import run on a machine with nothing new to
    // collect would report success while archiving nothing.
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).expect("fixture stage");
    let (archive, key) = archive(sandbox.root());

    let error = archive
        .push(&stage, &key)
        .expect_err("a stage with neither shards nor metadata must not push");
    assert!(error.to_string().contains("empty snapshot"), "{error}");

    let export = source(sandbox.root(), DEEPSEEK_NAME, DEEPSEEK_CANARY);
    raw_export::seal_export_file(&stage, MACHINE, "deepseek", &export).expect("seal");
    assert_eq!(
        store::sealed_shard_count(&stage).expect("count session shards"),
        0,
        "the export object is not a session shard"
    );

    let summary = archive
        .push(&stage, &key)
        .expect("push the export-only stage");
    assert_eq!(summary.stage_shards, 0);
    assert!(summary.files_new > 0, "the object itself is new content");
    let archived = raw_export::read_archived_export(
        &archive,
        &key,
        MACHINE,
        "deepseek",
        DEEPSEEK_SHA,
        &mut io::sink(),
    )
    .expect("read the object back from an export-only push");
    assert_eq!(archived.content_sha256, DEEPSEEK_SHA);
    assert_eq!(archived.bytes, DEEPSEEK_CANARY.len() as u64);
}

#[test]
fn a_source_that_cannot_be_read_is_never_reported_as_archived_nothing() {
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).expect("fixture stage");
    let missing = sandbox.root().join("Downloads").join(DEEPSEEK_NAME);

    let error = raw_export::seal_export_file(&stage, MACHINE, "deepseek", &missing)
        .expect_err("an unreadable export is a failure");
    // Exit 3 territory: the CLI reads a root `std::io::Error` as "did not
    // finish reading", which is not the same verdict as "archived 0".
    assert!(
        error.downcast_ref::<io::Error>().is_some(),
        "the io::Error must survive the context chain, or the CLI cannot tell \
         did-not-finish from failed: {error}"
    );
    assert_eq!(
        archived_objects(&stage),
        Vec::<(String, String, String)>::new()
    );
    assert!(matches!(
        raw_export::read_export_index(&stage, MACHINE).unwrap(),
        ExportIndexState::Missing
    ));

    // A label that would escape the namespace is a usage error, not a write.
    let error = raw_export::seal_export_file(&stage, MACHINE, "../../etc", &missing)
        .expect_err("a traversal label must be refused");
    assert!(
        error
            .downcast_ref::<raw_export::ExportLabelError>()
            .is_some(),
        "{error}"
    );
    assert!(!raw_export::export_files_root(&stage).exists());
}
