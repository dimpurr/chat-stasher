//! ISSUE-006 step C (W1055, ADR-063): a pack is published by tmp, verify,
//! rename, and tmp debris is swept by name.
//!
//! One section per ruling the principal approved on 2026-10-28
//! (the ADR-063 review), each checked to fail when its mechanism is removed:
//!
//! * **M1** · a repository holding a `.tmp-*` file completes `push`, `read`,
//!   `verify`, `prune-orphans` and `mirror sync`, and third-party listers
//!   (rustic's own backend listers and `check`, plus a stock `restic check`
//!   when one is installed) do not error on it. This fixture decides D1a's
//!   tmp location.
//! * **M2** · the final name already exists: equal hash ⇒ only our tmp is
//!   deleted and the publish succeeds; different bytes ⇒ nothing is touched
//!   and the push stops.
//! * **1** · the read-back is unconditional, goes to the backend and hashes to
//!   the id.
//! * **2** · only a push connection deletes; read-only commands never do.
//! * **3** · the older spellings are inventoried and aged by name; the
//!   destination no longer defaults `atomic_write_dir`.
//! * **4** · the name is `<id>.tmp-<unix>-<16 hex>` with an OS random part.
//! * **5** · both the name's timestamp and the backend mtime must exceed seven
//!   days; either unreadable keeps the file. kill-100 runs on both covered
//!   families.
//! * **6** · doctor and `prune-orphans` print the tmp line; a healthy
//!   repository prints nothing.
//!
//! The SFTP family cannot run a real server in the gate (orphan-packs.md's
//! honesty note), so its kill-100 drives the same publication code through an
//! IO with SFTP v3's rename semantics (a rename onto an existing name fails)
//! and non-atomic writes. The real storage-box run is the release blocker.
//!
//! Everything is local, synthetic and sandboxed (W306).

use bytes::Bytes;
use chat_stasher::atomic_pack::{
    classify, parse_tmp_name, publish, survey, sweep, tmp_name, Age, AtomicPackBackend, FileEntry,
    OperatorIo, PackIo, PublishError, Published, TmpKind, SWEEP_AGE_SECS,
};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use rustic_core::{FileType, Id, ReadBackend, RusticResult, WriteBackend};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../src/test_support.rs"]
mod test_support;

// ------------------------------------------------------------- helpers

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Pack-shaped bytes of a given seed: the id is their SHA-256, like a pack's.
fn pack(seed: usize) -> (String, Bytes) {
    let body = format!("synthetic pack {seed} ").repeat(64 + seed % 17);
    let bytes = Bytes::from(body.into_bytes());
    (sha_hex(&bytes), bytes)
}

fn final_rel(id: &str) -> String {
    format!("data/{}/{}", &id[..2], id)
}

fn set_mtime(path: &Path, secs: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

/// Every file under `data/` of a repository root, relative path -> bytes.
fn data_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// The invariant the whole change exists for: every file wearing a pack's
/// final name is complete (hashes to that name), and everything else in
/// `data/` is tmp-named. Returns the number of tmp-named leftovers.
fn assert_namespace_clean(root: &Path, context: &str) -> usize {
    let mut tmp = 0;
    for (rel, bytes) in data_files(root) {
        let name = rel.rsplit('/').next().unwrap().to_string();
        if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            assert_eq!(
                sha_hex(&bytes),
                name,
                "{context}: a final-named pack must be complete"
            );
        } else {
            assert!(
                classify(&rel).is_some(),
                "{context}: `{rel}` is neither a complete pack nor tmp debris"
            );
            tmp += 1;
        }
    }
    tmp
}

// ------------------------------------------------- fault-injecting IO

/// Where a simulated process dies.
#[derive(Clone, Copy, Debug)]
enum Kill {
    /// Never.
    Never,
    /// Mid-write, after this many bytes reached the backend.
    MidWrite(usize),
    /// Right after the tmp is fully written, before the read-back.
    BeforeVerify,
    /// After a passed verification, before the rename.
    BeforeRename,
}

/// A [`PackIo`] over a real directory with a process that can die, SFTP v3
/// rename semantics on request, and a few misbehaviours the fixtures need.
struct Rig {
    inner: OperatorIo,
    kill: Kill,
    dead: AtomicBool,
    /// SFTP v3: a rename onto an existing name fails.
    strict_rename: bool,
    /// The next `exists(final)` lies "no" once: a racing writer publishes
    /// between our check and our rename.
    lie_once: AtomicBool,
    /// Reads of tmp files come back with a flipped byte.
    corrupt_tmp_reads: bool,
    /// Listings carry no mtime.
    no_mtime: bool,
    /// Every write lands part of the file and then fails, but the process
    /// lives (a dropped connection, not a kill).
    fail_writes: bool,
    log: Mutex<Vec<String>>,
}

impl Rig {
    fn new(root: &Path) -> Self {
        let mut options = BTreeMap::new();
        options.insert("root".to_string(), root.to_string_lossy().into_owned());
        Self {
            inner: OperatorIo::open("fs", options).unwrap(),
            kill: Kill::Never,
            dead: AtomicBool::new(false),
            strict_rename: false,
            lie_once: AtomicBool::new(false),
            corrupt_tmp_reads: false,
            no_mtime: false,
            fail_writes: false,
            log: Mutex::new(Vec::new()),
        }
    }

    fn sftp_shaped(root: &Path) -> Self {
        let mut rig = Self::new(root);
        rig.strict_rename = true;
        rig
    }

    fn killed_at(mut self, kill: Kill) -> Self {
        self.kill = kill;
        self
    }

    fn note(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn alive(&self) -> anyhow::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            anyhow::bail!("the process is dead");
        }
        Ok(())
    }

    fn die(&self) -> anyhow::Error {
        self.dead.store(true, Ordering::SeqCst);
        anyhow::anyhow!("killed")
    }
}

impl PackIo for Rig {
    fn write(&self, path: &str, bytes: Bytes) -> anyhow::Result<()> {
        self.alive()?;
        self.note(format!("write {path}"));
        if self.fail_writes {
            self.inner.write(path, bytes.slice(..bytes.len() / 2))?;
            anyhow::bail!("injected connection failure");
        }
        if let Kill::MidWrite(n) = self.kill {
            let n = n.min(bytes.len().saturating_sub(1));
            self.inner.write(path, bytes.slice(..n))?;
            return Err(self.die());
        }
        self.inner.write(path, bytes)?;
        if matches!(self.kill, Kill::BeforeVerify) {
            return Err(self.die());
        }
        Ok(())
    }

    fn read(&self, path: &str) -> anyhow::Result<Bytes> {
        self.alive()?;
        self.note(format!("read {path}"));
        let bytes = self.inner.read(path)?;
        if self.corrupt_tmp_reads && classify(path).is_some() {
            let mut flipped = bytes.to_vec();
            flipped[0] ^= 0xff;
            return Ok(Bytes::from(flipped));
        }
        Ok(bytes)
    }

    fn exists(&self, path: &str) -> anyhow::Result<bool> {
        self.alive()?;
        if classify(path).is_none() && self.lie_once.swap(false, Ordering::SeqCst) {
            return Ok(false);
        }
        self.inner.exists(path)
    }

    fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        self.alive()?;
        self.note(format!("rename {from} -> {to}"));
        if matches!(self.kill, Kill::BeforeRename) {
            return Err(self.die());
        }
        if self.strict_rename && self.inner.exists(to)? {
            anyhow::bail!("rename failed: the target exists (SFTP v3)");
        }
        self.inner.rename(from, to)
    }

    fn delete(&self, path: &str) -> anyhow::Result<()> {
        self.alive()?;
        self.note(format!("delete {path}"));
        self.inner.delete(path)
    }

    fn list_files(&self, dir: &str) -> anyhow::Result<Vec<FileEntry>> {
        self.alive()?;
        let mut files = self.inner.list_files(dir)?;
        if self.no_mtime {
            for file in &mut files {
                file.mtime = None;
            }
        }
        Ok(files)
    }
}

// ----------------------------------------------------- name format (4)

#[test]
fn ruling4_the_tmp_name_is_id_unix_and_sixteen_hex_and_the_random_part_is_the_os() {
    let (id, bytes) = pack(1);
    let dir = tempfile::tempdir().unwrap();
    let rig = Arc::new(Rig::new(dir.path()));
    // Two backend writes of the same pack through the real wrapper: the inner
    // backend must never see a pack, and the two tmp names must differ only in
    // their random part.
    let inner = Arc::new(Recorder::default());
    let io = Arc::clone(&rig);
    let backend = AtomicPackBackend::around(
        inner.clone(),
        Box::new(move || {
            let io: Arc<dyn PackIo> = io.clone();
            Ok(io)
        }),
    );
    let before = unix_now();
    let parsed: Id = id.parse().unwrap();
    backend
        .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
        .unwrap();
    // A second publication of the same id: the final name now exists with
    // equal bytes (M2), and its tmp name must still be a fresh one.
    backend
        .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
        .unwrap();
    let after = unix_now();

    let tmps: Vec<String> = rig
        .log()
        .into_iter()
        .filter_map(|l| l.strip_prefix("write ").map(str::to_string))
        .collect();
    assert_eq!(tmps.len(), 2, "two publications, two tmp writes");
    let mut randoms = Vec::new();
    for tmp in &tmps {
        assert_eq!(classify(tmp).map(|c| c.0), Some(TmpKind::Current));
        let name = tmp.rsplit('/').next().unwrap();
        let (found_id, stamp) = parse_tmp_name(name).expect("current format");
        assert_eq!(found_id, id);
        assert!(
            (before..=after).contains(&stamp),
            "the stamp is upload start"
        );
        assert!(
            tmp.starts_with(&format!("data/{}/", &id[..2])),
            "next to the final"
        );
        randoms.push(name.rsplit('-').next().unwrap().to_string());
    }
    assert_eq!(randoms[0].len(), 16);
    assert_ne!(
        randoms[0], randoms[1],
        "one fresh random suffix per attempt"
    );
    assert_eq!(
        inner.pack_writes.load(Ordering::SeqCst),
        0,
        "no pack takes the inner path"
    );

    // Everything that is not a pack still takes the inner path untouched.
    backend
        .write_bytes(FileType::Index, &parsed, true, Bytes::from_static(b"idx"))
        .unwrap();
    assert_eq!(inner.other_writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        tmp_name(&id, 7, [0; 8]),
        format!("{id}.tmp-7-0000000000000000")
    );
}

/// A backend that records what reaches it. Reads panic: publication must
/// verify through the writer's own IO and never through anything else.
#[derive(Debug, Default)]
struct Recorder {
    pack_writes: AtomicUsize,
    other_writes: AtomicUsize,
}

impl ReadBackend for Recorder {
    fn location(&self) -> String {
        "recorder".into()
    }
    fn list_with_size(&self, _: FileType) -> RusticResult<Vec<(Id, u32)>> {
        panic!("publication must not list through the inner backend")
    }
    fn read_full(&self, _: FileType, _: &Id) -> RusticResult<Bytes> {
        panic!("the read-back must not go through the inner backend or any cache above it")
    }
    fn read_partial(&self, _: FileType, _: &Id, _: bool, _: u32, _: u32) -> RusticResult<Bytes> {
        panic!("the read-back must not go through the inner backend or any cache above it")
    }
    fn warmup_path(&self, _: FileType, _: &Id) -> String {
        String::new()
    }
}

impl WriteBackend for Recorder {
    fn write_bytes(&self, tpe: FileType, _: &Id, _: bool, _: Bytes) -> RusticResult<()> {
        if tpe == FileType::Pack {
            self.pack_writes.fetch_add(1, Ordering::SeqCst);
        } else {
            self.other_writes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn remove(&self, _: FileType, _: &Id, _: bool) -> RusticResult<()> {
        Ok(())
    }
}

// ----------------------------------------------- verification (ruling 1)

#[test]
fn ruling1_the_pack_is_read_back_from_the_backend_and_hashed_before_the_rename() {
    let (id, bytes) = pack(2);
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(dir.path());
    let outcome = publish(&rig, &id, bytes.clone(), 100, [1; 8]).unwrap();
    assert_eq!(outcome, Published::Renamed);
    let log = rig.log();
    let tmp = tmp_name(&id, 100, [1; 8]);
    let tmp_path = format!("data/{}/{tmp}", &id[..2]);
    // write tmp, read tmp back, then rename: that order, nothing else.
    assert_eq!(
        log,
        vec![
            format!("write {tmp_path}"),
            format!("read {tmp_path}"),
            format!("rename {tmp_path} -> {}", final_rel(&id)),
        ]
    );
    assert_eq!(
        fs::read(dir.path().join(final_rel(&id))).unwrap(),
        bytes.to_vec()
    );
    assert_eq!(assert_namespace_clean(dir.path(), "after publish"), 0);
}

#[test]
fn ruling1_bytes_that_do_not_hash_to_the_id_are_never_published() {
    let (id, bytes) = pack(3);
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path());
    rig.corrupt_tmp_reads = true; // the backend hands back damaged bytes
    let error = publish(&rig, &id, bytes, 100, [2; 8]).unwrap_err();
    assert!(matches!(error, PublishError::VerifyMismatch), "{error:?}");
    assert!(
        !dir.path().join(final_rel(&id)).exists(),
        "a pack that failed verification must not exist under its final name"
    );
    assert_eq!(
        assert_namespace_clean(dir.path(), "after refusal"),
        0,
        "and our tmp is gone"
    );
}

// ----------------------------------------------------- M2 collisions

fn collision_fixture(rig_for: impl Fn(&Path) -> Rig, equal: bool) {
    let (id, bytes) = pack(4);
    let dir = tempfile::tempdir().unwrap();
    let existing: Vec<u8> = if equal {
        bytes.to_vec()
    } else {
        b"someone wrote this formal name wrong".to_vec()
    };
    let final_path = dir.path().join(final_rel(&id));
    fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    fs::write(&final_path, &existing).unwrap();

    let rig = rig_for(dir.path());
    let result = publish(&rig, &id, bytes, 100, [3; 8]);
    assert_eq!(
        fs::read(&final_path).unwrap(),
        existing,
        "the final file is never rewritten, in either branch"
    );
    let tmp_left = data_files(dir.path())
        .keys()
        .filter(|k| classify(k).is_some())
        .count();
    if equal {
        assert_eq!(result.unwrap(), Published::AlreadyPresent);
        assert_eq!(tmp_left, 0, "equal hash: only our own tmp is deleted");
    } else {
        assert!(matches!(result.unwrap_err(), PublishError::Collision));
        assert_eq!(
            tmp_left, 1,
            "different bytes: nothing deleted, our verified tmp stays"
        );
    }
}

#[test]
fn m2_a_pre_existing_identical_final_file_counts_as_published_on_both_families() {
    collision_fixture(Rig::new, true);
    collision_fixture(Rig::sftp_shaped, true);
}

#[test]
fn m2_a_pre_existing_different_final_file_stops_the_push_and_is_not_touched() {
    collision_fixture(Rig::new, false);
    collision_fixture(Rig::sftp_shaped, false);
}

#[test]
fn m2_a_writer_that_wins_the_race_between_our_check_and_our_rename_is_settled_the_same_way() {
    let (id, bytes) = pack(5);
    let dir = tempfile::tempdir().unwrap();
    let final_path = dir.path().join(final_rel(&id));
    fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    fs::write(&final_path, &bytes).unwrap();
    // The check says "free", the strict rename then fails because a racing
    // writer published first: the failure must be settled by hash, not
    // reported as an error.
    let rig = Rig::sftp_shaped(dir.path());
    rig.lie_once.store(true, Ordering::SeqCst);
    assert_eq!(
        publish(&rig, &id, bytes, 100, [4; 8]).unwrap(),
        Published::AlreadyPresent
    );
    assert_eq!(assert_namespace_clean(dir.path(), "after race"), 0);
}

// ------------------------------------------------- kill-100 (ruling 5)

fn kill_100(rig_for: impl Fn(&Path, Kill) -> Rig, family: &str) {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..100 {
        let (id, bytes) = pack(i + 10);
        let kill = match i % 3 {
            0 => Kill::MidWrite((i * 37) % bytes.len()),
            1 => Kill::BeforeVerify,
            _ => Kill::BeforeRename,
        };
        // The process dies at `kill`: it cannot clean up after itself.
        let doomed = rig_for(dir.path(), kill);
        let error = publish(&doomed, &id, bytes.clone(), 1_000 + i as u64, [i as u8; 8])
            .expect_err("a killed publication does not succeed");
        assert!(
            matches!(error, PublishError::Io(_)),
            "{family} #{i}: {error:?}"
        );
        assert!(
            !dir.path().join(final_rel(&id)).exists(),
            "{family} #{i}: a killed push left something under a final name"
        );
        assert_namespace_clean(dir.path(), &format!("{family} after kill #{i}"));
        // The re-push after the kill succeeds and reads back byte-identical.
        let again = rig_for(dir.path(), Kill::Never);
        assert_eq!(
            publish(
                &again,
                &id,
                bytes.clone(),
                2_000 + i as u64,
                [i as u8 + 1; 8]
            )
            .unwrap(),
            Published::Renamed,
            "{family} #{i}"
        );
        assert_eq!(
            fs::read(dir.path().join(final_rel(&id))).unwrap(),
            bytes.to_vec()
        );
    }
    // 100 kills left only tmp-named leftovers (zero partial finals, checked
    // after every kill above) and the by-name sweep clears them after 7 days.
    let leftovers = assert_namespace_clean(dir.path(), family);
    assert_eq!(
        leftovers, 100,
        "{family}: one tmp-named leftover per kill, nothing else"
    );
    let sweeper = Rig::new(dir.path());
    let report = sweep(&sweeper, unix_now() + SWEEP_AGE_SECS + 3_600);
    assert_eq!(
        report.removed, 100,
        "{family}: the sweep removes them all once old"
    );
    assert_eq!(assert_namespace_clean(dir.path(), family), 0);
}

#[test]
fn ruling5_kill_100_on_the_local_directory_family_leaves_no_partial_final_file() {
    kill_100(
        |root, kill| Rig::new(root).killed_at(kill),
        "local-directory",
    );
}

#[test]
fn ruling5_kill_100_on_the_sftp_family_leaves_no_partial_final_file() {
    kill_100(
        |root, kill| Rig::sftp_shaped(root).killed_at(kill),
        "sftp-shaped (real server: release blocker, not in-gate)",
    );
}

// -------------------------------------------------- sweep (rulings 2,3,5)

fn plant(root: &Path, rel: &str, bytes: &[u8], mtime: u64) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    set_mtime(&path, mtime);
}

const LEGACY_ID: &str = "cd00000000000000000000000000000000000000000000000000000000000002";

#[test]
fn ruling5_both_the_name_and_the_mtime_must_be_older_than_seven_days() {
    let dir = tempfile::tempdir().unwrap();
    let now = unix_now();
    let old = now - SWEEP_AGE_SECS - 86_400;
    let young = now - 3_600;
    let (id, _) = pack(20);
    let name = |stamp: u64, r: u8| format!("data/{}/{}", &id[..2], tmp_name(&id, stamp, [r; 8]));
    plant(dir.path(), &name(old, 1), b"both old", old);
    plant(dir.path(), &name(old, 2), b"name old, mtime young", young);
    plant(dir.path(), &name(young, 3), b"name young, mtime old", old);
    plant(dir.path(), &name(young, 4), b"both young", young);
    let (real, real_bytes) = pack(21);
    plant(dir.path(), &final_rel(&real), &real_bytes, old);
    plant(dir.path(), "data/ab/notes.txt", b"not ours", old);

    let report = sweep(&Rig::new(dir.path()), now);
    assert_eq!((report.removed, report.kept_young), (1, 3), "{report:?}");
    let left = data_files(dir.path());
    assert!(!left.contains_key(&name(old, 1)), "both old: removed");
    for kept in [name(old, 2), name(young, 3), name(young, 4)] {
        assert!(
            left.contains_key(&kept),
            "{kept}: the conservative clock keeps it"
        );
    }
    assert!(
        left.contains_key(&final_rel(&real)),
        "a final-named pack is never touched"
    );
    assert!(
        left.contains_key("data/ab/notes.txt"),
        "a foreign file is never touched"
    );
}

#[test]
fn ruling5_an_unreadable_age_keeps_the_file_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let now = unix_now();
    let old = now - SWEEP_AGE_SECS - 86_400;
    let (id, _) = pack(22);
    let path = format!("data/{}/{}", &id[..2], tmp_name(&id, old, [9; 8]));
    plant(dir.path(), &path, b"x", old);
    plant(dir.path(), &format!("data/cd/{LEGACY_ID}-tmp-"), b"x", old);
    let mut rig = Rig::new(dir.path());
    rig.no_mtime = true; // the backend will not say how old anything is
    let report = sweep(&rig, now);
    assert_eq!(
        (report.removed, report.kept_unknown_age),
        (0, 2),
        "{report:?}"
    );
    assert_eq!(data_files(dir.path()).len(), 2);
    // And the survey reports the same files as unknown-age, not as young.
    let found = survey(&rig);
    assert!(found.entries.iter().all(|e| e.age(now) == Age::Unknown));
}

#[test]
fn ruling3_the_older_spellings_are_inventoried_and_aged_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let now = unix_now();
    let old = now - SWEEP_AGE_SECS - 86_400;
    let young = now - 60;
    plant(
        dir.path(),
        &format!("data/cd/{LEGACY_ID}-tmp-"),
        b"local, old",
        old,
    );
    plant(
        dir.path(),
        &format!("index/cd/{LEGACY_ID}-tmp-"),
        b"local, young",
        young,
    );
    plant(
        dir.path(),
        ".chat-stasher-tmp/0123456789",
        b"staging, old",
        old,
    );
    plant(
        dir.path(),
        ".chat-stasher-tmp/sub/9876543210",
        b"staging, young",
        young,
    );
    plant(dir.path(), "config", b"a real file", old);

    let found = survey(&Rig::new(dir.path()));
    let kinds: Vec<TmpKind> = found.entries.iter().map(|e| e.kind).collect();
    assert_eq!(found.files(), 4, "{kinds:?}");
    assert_eq!(found.due(now), 2);
    assert_eq!(found.unlisted, 0);

    let report = sweep(&Rig::new(dir.path()), now);
    assert_eq!((report.removed, report.kept_young), (2, 2));
    assert!(dir.path().join("config").exists());
    assert!(dir
        .path()
        .join(format!("index/cd/{LEGACY_ID}-tmp-"))
        .exists());
    assert!(!dir
        .path()
        .join(format!("data/cd/{LEGACY_ID}-tmp-"))
        .exists());
}

// ------------------------------------------- end to end, real push path

fn store_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo_path.to_string_lossy().into_owned(),
        key_file: key_path.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache (W289).
        cache_dir: None,
        no_cache: true,
    }
}

fn opendal_fs_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: "opendal:fs".to_string(),
        options: BTreeMap::from([("root".to_string(), repo_path.to_string_lossy().into_owned())]),
        ..store_config(repo_path, key_path)
    }
}

fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

struct Sandbox {
    dir: tempfile::TempDir,
    mk: MasterKey,
}

impl Sandbox {
    fn repo(&self) -> PathBuf {
        self.dir.path().join("remote-repo")
    }
    fn key(&self) -> PathBuf {
        self.dir.path().join("remote.key")
    }
    fn stage(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn command(&self) -> std::process::Command {
        let home = self.dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&home),
            )
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env(
                "CHAT_STASHER_REGISTRY",
                self.dir.path().join("registry.json"),
            )
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    fn write_config(&self) {
        let config_dir = self.dir.path().join("config").join("chat-stasher");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[destinations.storagebox]\nrepo = '{}'\nkey_file = '{}'\nno_cache = \
                 true\n\n[mirror]\nenabled = true\nmirror_of = 'storagebox'\nmin_free_gib = \
                 1.0\n",
                self.repo().display(),
                self.key().display()
            ),
        )
        .unwrap();
        fs::write(
            self.dir.path().join("registry.json"),
            r#"{"schema_version":1,"generated":"W1055 synthetic","harnesses":[]}"#,
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> (i32, String) {
        let out = self.command().args(args).output().unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), all)
    }
}

/// A two-machine synthetic repository pushed through the real product push
/// path (the wrapper is in the stack), on the plain local shape.
fn sandbox_with_repository() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let cfg = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    for (stage, machine) in [("stage-a", "machine-a"), ("stage-b", "machine-b")] {
        write_session(&dir.path().join(stage), machine, "session-a1", 3);
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(&dir.path().join(stage), &mk)
            .unwrap();
    }
    let sandbox = Sandbox { dir, mk };
    sandbox.write_config();
    sandbox
}

fn pack_hashes(root: &Path) -> BTreeMap<String, String> {
    data_files(root)
        .into_iter()
        .filter(|(rel, _)| classify(rel).is_none())
        .map(|(rel, bytes)| (rel, sha_hex(&bytes)))
        .collect()
}

#[test]
fn the_real_push_publishes_every_pack_through_the_wrapper_on_both_local_shapes() {
    // Plain local path.
    let sandbox = sandbox_with_repository();
    assert!(
        !pack_hashes(&sandbox.repo()).is_empty(),
        "packs were pushed"
    );
    assert_eq!(assert_namespace_clean(&sandbox.repo(), "local path"), 0);

    // `opendal:fs`: the destination no longer defaults a staging directory
    // (ruling 3), and its packs are published the same way.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let cfg = opendal_fs_config(&repo, &dir.path().join("key"));
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    write_session(&dir.path().join("stage"), "machine-a", "session-a1", 3);
    let store = BackupStore::new(cfg.clone(), "machine-a".to_string());
    store.push(&dir.path().join("stage"), &mk).unwrap();
    assert!(!pack_hashes(&repo).is_empty());
    assert_eq!(assert_namespace_clean(&repo, "opendal:fs"), 0);
    // Ruling 3 as amended: the default staging directory stays, because index
    // and snapshot files still depend on it. Packs go through the wrapper
    // with it set, and nothing is left staged after a clean push.
    assert!(cfg.backend_options().contains_key("atomic_write_dir"));
    let mut staged: Vec<String> = data_files(&repo)
        .keys()
        .filter(|name| classify(name).is_some())
        .cloned()
        .collect();
    if let Ok(dir) = fs::read_dir(repo.join(".chat-stasher-tmp")) {
        staged.extend(dir.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()));
    }
    assert!(
        staged.is_empty(),
        "nothing is left staged after a push: {staged:?}"
    );
    store.open_indexed(&mk).expect("the repository reads back");
}

#[test]
fn m1_a_tmp_file_in_data_stops_nothing_and_third_party_listers_ignore_it() {
    let sandbox = sandbox_with_repository();
    let packs_before = pack_hashes(&sandbox.repo());
    // A young tmp (kept by every sweep) and an old one, both inside `data/`.
    let (id, _) = pack(30);
    let young = format!("data/{}/{}", &id[..2], tmp_name(&id, unix_now(), [7; 8]));
    plant(&sandbox.repo(), &young, &[0x44; 777], unix_now());

    // 1. rustic's own backend listers, the code a stock rustic/restic-style
    //    tool runs, neither error nor report the tmp as a pack.
    for opts in [
        rustic_backend::BackendOptions::default().repository(sandbox.repo().to_str().unwrap()),
        rustic_backend::BackendOptions::default()
            .repository("opendal:fs")
            .options(BTreeMap::from([(
                "root".to_string(),
                sandbox.repo().to_string_lossy().into_owned(),
            )])),
    ] {
        let backends = opts.to_backends().unwrap();
        let listed = backends
            .repository()
            .list_with_size(FileType::Pack)
            .unwrap();
        let ids: Vec<String> = listed
            .iter()
            .map(|(id, _)| id.to_hex().to_string())
            .collect();
        let expected: Vec<String> = packs_before
            .keys()
            .map(|rel| rel.rsplit('/').next().unwrap().to_string())
            .collect();
        let mut got = ids.clone();
        got.sort();
        let mut want = expected;
        want.sort();
        assert_eq!(
            got, want,
            "the lister ignores a non-id name instead of failing"
        );
    }

    // 2. rustic's own `check` over the same repository opens and passes.
    let cfg = store_config(&sandbox.repo(), &sandbox.key());
    {
        use rustic_core::{CheckOptions, Credentials, Repository, RepositoryOptions};
        let backends = rustic_backend::BackendOptions::default()
            .repository(sandbox.repo().to_str().unwrap())
            .to_backends()
            .unwrap();
        let repo = Repository::new(&RepositoryOptions::default().no_cache(true), &backends)
            .unwrap()
            .open(&Credentials::Masterkey(sandbox.mk.clone()))
            .unwrap();
        repo.check(CheckOptions::default().read_data(true))
            .expect("rustic check passes with a tmp file in data/");
    }

    // 3. a stock restic CLI, when one is installed: a restic-made repository
    //    with a `.tmp-*` planted in its data/ passes `restic check`.
    if let Ok(version) = std::process::Command::new("restic").arg("version").output() {
        if version.status.success() {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("restic-repo");
            let source = dir.path().join("source");
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("a.txt"), "synthetic restic payload".repeat(500)).unwrap();
            let restic = |args: &[&str]| {
                std::process::Command::new("restic")
                    .env("RESTIC_PASSWORD", "synthetic-password")
                    .env("RESTIC_CACHE_DIR", dir.path().join("restic-cache"))
                    .arg("-r")
                    .arg(&repo)
                    .args(args)
                    .output()
                    .unwrap()
            };
            assert!(restic(&["init"]).status.success());
            assert!(restic(&["backup", source.to_str().unwrap()])
                .status
                .success());
            assert!(
                restic(&["check", "--read-data"]).status.success(),
                "baseline"
            );
            let some_pack = data_files(&repo)
                .keys()
                .next()
                .cloned()
                .expect("restic wrote a pack");
            let id = some_pack.rsplit('/').next().unwrap().to_string();
            let tmp = format!("data/{}/{}", &id[..2], tmp_name(&id, unix_now(), [8; 8]));
            plant(&repo, &tmp, b"half a pack", unix_now());
            let checked = restic(&["check", "--read-data"]);
            assert!(
                checked.status.success(),
                "stock restic must tolerate a .tmp-* in data/ (D1a gate): {}",
                String::from_utf8_lossy(&checked.stderr)
            );
        }
    } else {
        eprintln!("m1: restic is not installed here; the stock-CLI leg did not run");
    }

    // 4. our own five commands complete as normal over the planted tmp.
    let stage = sandbox.stage("stage-a");
    write_session(&stage, "machine-a", "session-a2", 2);
    let out = |args: &[&str]| sandbox.run(args);
    let (code, all) = out(&["mirror", "sync", "--budget-secs", "120"]);
    assert_eq!(code, 0, "mirror sync over a tmp file:\n{all}");
    let (code, all) = out(&[
        "read",
        "--destination",
        "storagebox",
        "--machine",
        "machine-a",
        "--session",
        "session-a1",
    ]);
    assert_eq!(code, 0, "read over a tmp file:\n{all}");
    let (code, all) = out(&[
        "verify",
        "--level",
        "l2",
        "--destination",
        "storagebox",
        "--machine",
        "machine-a",
    ]);
    assert_eq!(code, 0, "verify over a tmp file:\n{all}");
    let (code, all) = out(&["prune-orphans", "--destination", "storagebox"]);
    assert_eq!(code, 0, "prune-orphans over a tmp file:\n{all}");
    assert!(
        !all.contains("torn") || all.contains("0 torn"),
        "a tmp file is not a torn pack:\n{all}"
    );
    BackupStore::new(cfg, "machine-a".to_string())
        .push(&stage, &sandbox.mk)
        .expect("push over a tmp file");
    assert!(
        sandbox.repo().join(&young).exists(),
        "a young tmp survives every command, the push included"
    );
}

#[test]
fn ruling2_only_a_push_connection_deletes() {
    let sandbox = sandbox_with_repository();
    let now = unix_now();
    let old = now - SWEEP_AGE_SECS - 86_400;
    let (id, _) = pack(31);
    let old_tmp = format!("data/{}/{}", &id[..2], tmp_name(&id, old, [5; 8]));
    plant(&sandbox.repo(), &old_tmp, b"half a pack", old);
    let legacy = format!("data/cd/{LEGACY_ID}-tmp-");
    plant(&sandbox.repo(), &legacy, b"half a pack", old);

    // Every read-only command, repeatedly: nothing is deleted.
    for args in [
        vec![
            "read",
            "--destination",
            "storagebox",
            "--machine",
            "machine-a",
            "--session",
            "session-a1",
        ],
        vec![
            "verify",
            "--level",
            "l2",
            "--destination",
            "storagebox",
            "--machine",
            "machine-a",
        ],
        vec!["prune-orphans", "--destination", "storagebox"],
        vec!["prune-orphans", "--destination", "storagebox", "--json"],
        vec!["mirror", "sync", "--budget-secs", "120"],
        vec!["doctor"],
        vec!["status"],
    ] {
        let (code, all) = sandbox.run(&args);
        // `status` reports this machine's own health (never pushed here), so
        // its code is not the subject; a usage error (2) would be a test bug.
        assert!(code != 2, "{args:?} -> {code}:\n{all}");
        assert!(
            sandbox.repo().join(&old_tmp).exists(),
            "{args:?} deleted a tmp file"
        );
        assert!(
            sandbox.repo().join(&legacy).exists(),
            "{args:?} deleted a legacy tmp"
        );
    }
    // The push connection is where it goes, and only the tmp names: every
    // pack keeps its exact bytes.
    let packs = pack_hashes(&sandbox.repo());
    let cfg = store_config(&sandbox.repo(), &sandbox.key());
    BackupStore::new(cfg, "machine-a".to_string())
        .push(&sandbox.stage("stage-a"), &sandbox.mk)
        .unwrap();
    assert!(
        !sandbox.repo().join(&old_tmp).exists(),
        "the push swept the old tmp"
    );
    assert!(
        !sandbox.repo().join(&legacy).exists(),
        "and the old legacy spelling"
    );
    assert_eq!(pack_hashes(&sandbox.repo()), packs, "no pack was touched");
}

#[test]
fn ruling6_doctor_and_prune_orphans_show_a_tmp_line_and_a_healthy_repository_shows_none() {
    let sandbox = sandbox_with_repository();
    let tmp_line = "tmp debris";

    let (code, healthy_prune) = sandbox.run(&["prune-orphans", "--destination", "storagebox"]);
    assert_eq!(code, 0, "{healthy_prune}");
    let (code, healthy_doctor) = sandbox.run(&["doctor"]);
    assert_eq!(code, 0, "{healthy_doctor}");
    assert!(
        !healthy_prune.contains(tmp_line),
        "healthy prune-orphans:\n{healthy_prune}"
    );
    assert!(
        !healthy_doctor.contains("debris: tmp"),
        "healthy doctor:\n{healthy_doctor}"
    );

    let now = unix_now();
    let old = now - SWEEP_AGE_SECS - 86_400;
    let (id, _) = pack(32);
    let name = |stamp: u64, r: u8| format!("data/{}/{}", &id[..2], tmp_name(&id, stamp, [r; 8]));
    plant(&sandbox.repo(), &name(old, 1), &[1; 100], old);
    plant(&sandbox.repo(), &name(now, 2), &[2; 50], now);

    let (code, prune) = sandbox.run(&["prune-orphans", "--destination", "storagebox"]);
    assert_eq!(code, 0, "tmp debris is a known state:\n{prune}");
    assert!(
        prune.contains("[prune] tmp debris  : 2 leftover tmp file(s) (150 B)")
            && prune.contains("1 older than 7 days will be removed by the next push"),
        "prune-orphans words the tmp line:\n{prune}"
    );
    let (code, doctor) = sandbox.run(&["doctor"]);
    assert_eq!(code, 0, "{doctor}");
    assert!(
        doctor.contains("debris: tmp — 2 leftover tmp file(s) (150 B)"),
        "doctor words the same line:\n{doctor}"
    );
    let out = sandbox
        .command()
        .args(["prune-orphans", "--destination", "storagebox", "--json"])
        .output()
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(value["tmp_debris"]["kind"], "measured");
    assert_eq!(value["tmp_debris"]["files"], 2);
    assert_eq!(value["tmp_debris"]["bytes"], 150);
    assert_eq!(value["tmp_debris"]["due_for_removal"], 1);
}

#[test]
fn the_wrapper_covers_exactly_the_two_families_and_d1a_keeps_tmp_in_data() {
    // D1a's recorded decision: the M1 fixtures above passed with the tmp in
    // `data/<xx>/`, so that is where it stays. If this assertion ever has to
    // change, the M1 fixture failed and ADR-063 D1a's fallback applies.
    let (id, bytes) = pack(40);
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(dir.path());
    publish(&rig, &id, bytes, 5, [6; 8]).unwrap();
    let write = rig.log().into_iter().next().unwrap();
    assert!(
        write.starts_with(&format!("write data/{}/", &id[..2])),
        "{write}"
    );
}

// ------------------------------- amended ruling 3: staged index/snapshot

/// Child half of the kill fixture: write a 4 MB index file through rustic's
/// own `opendal:fs` backend with a 1 MB file-size limit, so the kernel kills
/// the process in the middle of the write (SIGXFSZ).
#[cfg(unix)]
fn killed_index_write_child(root: &str, keep_default_staging: bool) {
    let cfg = StoreConfig {
        repo_root: "opendal:fs".to_string(),
        options: BTreeMap::from([("root".to_string(), root.to_string())]),
        ..store_config(Path::new(root), Path::new("unused.key"))
    };
    let mut options = cfg.backend_options();
    if !keep_default_staging {
        options.remove("atomic_write_dir");
    }
    let backends = rustic_backend::BackendOptions::default()
        .repository("opendal:fs")
        .options(options)
        .to_backends()
        .unwrap();
    let id: Id = "cd".repeat(32).parse().unwrap();
    let limit = libc::rlimit {
        rlim_cur: 1_000_000,
        rlim_max: 1_000_000,
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain setrlimit calls on this child process with valid structs.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        libc::setrlimit(libc::RLIMIT_FSIZE, &limit);
    }
    backends
        .repository()
        .write_bytes(
            FileType::Index,
            &id,
            true,
            Bytes::from(vec![7_u8; 4_000_000]),
        )
        .unwrap();
    unreachable!("the write must have been killed by the file-size limit");
}

/// A writer killed in the middle of an index write must leave nothing under
/// the index's final name when the default staging directory is in force, and
/// the control run (staging removed) proves the fixture bites: there the
/// partial file sits under the final name. This is why the default stays.
#[cfg(unix)]
#[test]
fn m1_amended_a_killed_index_write_leaves_no_torn_index_under_its_final_name() {
    use std::os::unix::process::ExitStatusExt;
    if let Ok(mode) = std::env::var("W1055_KILL_CHILD") {
        let root = std::env::var("W1055_KILL_ROOT").unwrap();
        killed_index_write_child(&root, mode == "staged");
        return;
    }
    let run = |mode: &str| -> (PathBuf, std::process::ExitStatus, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("index")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "m1_amended_a_killed_index_write_leaves_no_torn_index_under_its_final_name",
                "--test-threads=1",
            ])
            .env("W1055_KILL_CHILD", mode)
            .env("W1055_KILL_ROOT", &root)
            .status()
            .unwrap();
        (root, status, dir)
    };
    let final_index = |root: &Path| -> Option<u64> {
        let path = root.join("index").join("cd".repeat(32));
        fs::metadata(path).ok().map(|m| m.len())
    };

    let (control_root, status, _keep) = run("control");
    assert_eq!(
        status.signal(),
        Some(libc::SIGXFSZ),
        "the control child was killed mid-write"
    );
    let torn =
        final_index(&control_root).expect("control: the partial file is under the final name");
    assert!(torn < 4_000_000, "control: it is partial ({torn} bytes)");

    let (staged_root, status, _keep2) = run("staged");
    assert_eq!(
        status.signal(),
        Some(libc::SIGXFSZ),
        "the staged child was killed mid-write"
    );
    assert_eq!(
        final_index(&staged_root),
        None,
        "with the default staging directory a killed write leaves nothing under the final name"
    );
    assert!(
        staged_root.join(".chat-stasher-tmp").exists(),
        "the partial file sits in the staging directory, which the by-name sweep covers"
    );
}

// ----------------------------------------------------- retry (review M2)

use chat_stasher::atomic_pack::RetryPolicy;

fn quick() -> RetryPolicy {
    RetryPolicy {
        attempts: 2,
        backoff: Duration::from_millis(1),
    }
}

/// A backend whose connections come from `make(connection_number)`.
fn backend_over(
    make: impl Fn(usize) -> anyhow::Result<Rig> + Send + Sync + 'static,
) -> (Arc<dyn WriteBackend>, Arc<AtomicUsize>) {
    let made = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&made);
    let backend = AtomicPackBackend::around_with_policy(
        Arc::new(Recorder::default()),
        Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let io: Arc<dyn PackIo> = Arc::new(make(n)?);
            Ok(io)
        }),
        quick(),
    );
    (backend, made)
}

fn tmp_count(root: &Path) -> usize {
    data_files(root)
        .keys()
        .filter(|k| classify(k).is_some())
        .count()
}

#[test]
fn m2_a_transient_failure_succeeds_on_retry_and_leaves_no_tmp() {
    let (id, bytes) = pack(50);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // The first connection drops mid-write; every later one is healthy.
    let (backend, made) = backend_over(move |n| {
        let mut rig = Rig::new(&root);
        rig.fail_writes = n == 0;
        Ok(rig)
    });
    let parsed: Id = id.parse().unwrap();
    backend
        .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
        .unwrap();
    assert_eq!(
        fs::read(dir.path().join(final_rel(&id))).unwrap(),
        bytes.to_vec()
    );
    assert_eq!(
        tmp_count(dir.path()),
        0,
        "the failed attempt's partial tmp was removed"
    );
    assert!(
        made.load(Ordering::SeqCst) >= 2,
        "the retry used a fresh connection"
    );
    assert_eq!(assert_namespace_clean(dir.path(), "after retry"), 0);
}

#[test]
fn m2_a_persistent_failure_is_worded_and_leaves_no_tmp_where_cleanup_can_reach() {
    let (id, bytes) = pack(51);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (backend, _) = backend_over(move |_| {
        let mut rig = Rig::new(&root);
        rig.fail_writes = true; // every connection drops mid-write, deletes work
        Ok(rig)
    });
    let parsed: Id = id.parse().unwrap();
    for push in 0..5 {
        let error = backend
            .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed after 2 attempt(s)"), "{error}");
        assert!(
            error.contains("Nothing was published under a final name"),
            "{error}"
        );
        assert!(error.contains("were removed"), "{error}");
        assert!(
            !error.contains("Unexpected"),
            "no bare opendal kind reaches the user: {error}"
        );
        assert_eq!(tmp_count(dir.path()), 0, "push {push}: nothing accumulates");
    }
    assert!(!dir.path().join(final_rel(&id)).exists());
}

#[test]
fn m2_when_no_fresh_connection_can_be_made_the_leftover_is_bounded_then_cleared() {
    let (id, bytes) = pack(52);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // Connection 0 drops mid-write; the destination then refuses new
    // connections for a while (1..=3), and later comes back.
    let (backend, _) = backend_over(move |n| match n {
        0 => {
            let mut rig = Rig::new(&root);
            rig.fail_writes = true;
            Ok(rig)
        }
        1..=3 => anyhow::bail!("injected: cannot connect"),
        _ => Ok(Rig::new(&root)),
    });
    let parsed: Id = id.parse().unwrap();
    let error = backend
        .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("could not be removed right now"), "{error}");
    assert_eq!(
        tmp_count(dir.path()),
        1,
        "one failed attempt, one leftover, no growth"
    );
    // A second push while the destination still refuses connections adds
    // nothing it cannot account for, and the first connection that works
    // clears everything this process left.
    // Still refused while the destination refuses connections: the result is
    // not the point here, the leftover count is.
    drop(backend.write_bytes(FileType::Pack, &parsed, false, bytes.clone()));
    backend
        .write_bytes(FileType::Pack, &parsed, false, bytes.clone())
        .unwrap();
    assert_eq!(
        tmp_count(dir.path()),
        0,
        "the recovered connection removed the earlier leftovers"
    );
    assert_eq!(
        fs::read(dir.path().join(final_rel(&id))).unwrap(),
        bytes.to_vec()
    );
}

#[test]
fn m2_a_collision_is_not_retried() {
    let (id, bytes) = pack(53);
    let dir = tempfile::tempdir().unwrap();
    let final_path = dir.path().join(final_rel(&id));
    fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    fs::write(&final_path, b"formal name written wrong").unwrap();
    let root = dir.path().to_path_buf();
    let (backend, made) = backend_over(move |_| Ok(Rig::new(&root)));
    let parsed: Id = id.parse().unwrap();
    let error = backend
        .write_bytes(FileType::Pack, &parsed, false, bytes)
        .unwrap_err()
        .to_string();
    assert!(error.contains("different bytes"), "{error}");
    assert_eq!(
        made.load(Ordering::SeqCst),
        1,
        "no second connection, no second attempt"
    );
}
