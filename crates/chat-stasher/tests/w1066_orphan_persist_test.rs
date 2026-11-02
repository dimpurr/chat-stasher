//! W1066: persisting the packs one open verified, as one new index file.
//!
//! What this file pins, at sizes the gate can afford:
//!
//! * **the persist itself** — a push that survives its adoption names the packs
//!   it verified in exactly one new index file, and a later push over the same
//!   repository names nothing again (the re-survey skip): adoption stops
//!   being per-open;
//! * **the one ruling change** — torn debris (an interrupted push's
//!   half-written file, the shape `crate::debris` classifies by its unreadable
//!   header) no longer refuses the complete packs around it: they are adopted
//!   and named, the debris is excluded, untouched and never named by any
//!   index file;
//! * **what did not change** — a complete pack whose bytes do not verify still
//!   refuses the whole adoption, and nothing is named then: the check on
//!   packs that could be content is not relaxed by the debris exclusion;
//! * **two clients** — two pushes parked so that both have surveyed and
//!   neither has written both persist anyway, which is the state no lock can
//!   prevent, and the repository they leave behind still reads and dedups:
//!   two index files name the same packs with identical entries.
//!
//! Every fixture is synthetic and local: a generated payload, an invented
//! machine, no registry, no network and no credentials.
//!
//! The tests are black-box on purpose: they assert on the repository a real
//! push leaves behind (survey statements, index-file naming counts, dedup
//! counters), not on the fields W1066 added, so the very tests that are red
//! before the fix are the ones that turn green with it.

use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::{IndexFile, MasterKey};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[path = "../src/test_support.rs"]
mod test_support;

// ------------------------------------------------------------- fixtures

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

/// A deterministic, poorly-compressible payload (the w245 filler, spelled here
/// so this file stands alone).
fn filler(bytes: usize, seed: u64) -> String {
    let mut state = seed | 1;
    let hex = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes);
    while out.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(hex[(state & 0xf) as usize] as char);
    }
    out
}

/// One machine's stage with `sessions` sealed shards of `bytes_each` bytes, all
/// distinct seeds, so chunking has real boundaries to find.
fn write_stage(dir: &Path, machine: &str, sessions: usize, bytes_each: usize) -> PathBuf {
    let stage = dir.join("stage");
    for i in 0..sessions {
        let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-{i:012}");
        let body = format!(
            r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1","timestamp":"2025-06-01T10:00:00Z"}}"#,
            filler(bytes_each, 0x9E37_79B9_7F4A_7C15 ^ (i as u64) * 0x517C_C1B7)
        ) + "\n";
        let dir = stage
            .join("sessions")
            .join(machine)
            .join(&session)
            .join("000");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000001.jsonl"), &body).unwrap();
    }
    stage
}

/// Every finished pack under `repo/data/`, as `(hex id, bytes)`, sorted.
fn final_packs(repo: &Path) -> Vec<(String, u64)> {
    let mut packs = Vec::new();
    let mut stack = vec![repo.join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                stack.push(path);
            } else if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
                packs.push((name, bytes));
            }
        }
    }
    packs.sort();
    packs
}

/// Drop the index and the snapshots, keeping `config` and every pack: the
/// deterministic state a push killed inside its pack window leaves behind. The
/// two directories are recreated empty, which is a listing-legal state a fresh
/// survey must read as "no index file at all".
fn strand_packs(repo: &Path) -> BTreeSet<String> {
    let packs = final_packs(repo);
    assert!(
        !packs.is_empty(),
        "the first push wrote no packs to strand — the fixture is broken"
    );
    for entry in ["index", "snapshots"] {
        let dir = repo.join(entry);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
    }
    packs.into_iter().map(|(id, _)| id).collect()
}

/// What a fresh survey sees: the ids named by no index file, and how many index
/// files exist at all. One private statement of the same facts every command
/// reads, so the assertions here describe the repository, not the API that
/// built it.
fn survey_unindexed(cfg: &StoreConfig, mk: &MasterKey) -> (BTreeSet<String>, usize) {
    let backends = BackupStore::new(cfg.clone(), "machine-a".to_string())
        .backends()
        .unwrap();
    let repo = chat_stasher::orphans::open_for_survey(cfg, &backends, mk).unwrap();
    let inventory = chat_stasher::orphans::survey_packs(&repo, &backends).unwrap();
    let mut index_files = 0usize;
    for file in repo.stream_files::<IndexFile>().unwrap() {
        let _ = file.unwrap();
        index_files += 1;
    }
    (
        inventory
            .report
            .unindexed
            .iter()
            .map(|(id, _)| id.clone())
            .collect(),
        index_files,
    )
}

/// How many index files name each target pack: `id -> count`. Two clients that
/// persist the same pack legitimately produce two files whose entries are
/// identical, so the counts this returns are the merge-collision census.
fn naming_files(
    cfg: &StoreConfig,
    mk: &MasterKey,
    targets: &BTreeSet<String>,
) -> BTreeMap<String, usize> {
    let backends = BackupStore::new(cfg.clone(), "machine-a".to_string())
        .backends()
        .unwrap();
    let repo = chat_stasher::orphans::open_for_survey(cfg, &backends, mk).unwrap();
    let mut counts: BTreeMap<String, usize> = targets.iter().map(|id| (id.clone(), 0)).collect();
    for file in repo.stream_files::<IndexFile>().unwrap() {
        let (_, index) = file.unwrap();
        let named: BTreeSet<String> = index
            .packs
            .iter()
            .chain(index.packs_to_delete.iter())
            .map(|pack| pack.id.to_hex().to_string())
            .collect();
        for id in targets {
            if named.contains(id) {
                *counts.get_mut(id).unwrap() += 1;
            }
        }
    }
    counts
}

/// Push once as `machine-a`, through the in-process store, and prove it
/// uploaded the fixture's data (so the stranded state below is real content).
fn seeding_push(cfg: &StoreConfig, stage: &Path, mk: &MasterKey) {
    let summary = BackupStore::new(cfg.clone(), "machine-a".to_string())
        .push(stage, mk)
        .unwrap();
    assert!(
        summary.data_added > 100_000,
        "the fixture's first push uploaded {} bytes, which is no payload to strand",
        summary.data_added
    );
}

/// The tree pack is the smallest finished pack (a whole payload rides in one
/// data pack at these sizes); picking it as the singled-out victim leaves the
/// big data pack in the asserted-adopted set, which keeps the choose-a-side
/// byte thresholds of the tests below far apart.
fn smallest_pack(repo: &Path) -> (String, u64, PathBuf) {
    let packs = final_packs(repo);
    let mut smallest = &packs[0];
    for pack in &packs {
        if pack.1 < smallest.1 {
            smallest = pack;
        }
    }
    let path = repo.join("data").join(&smallest.0[..2]).join(&smallest.0);
    (smallest.0.clone(), smallest.1, path)
}

// ------------------------------------------------- the persist itself

/// A push that adopts packs no index file names writes exactly one new index
/// file naming them, and the repository a later survey reads agrees: nothing
/// is unindexed any more. Red before W1066: no push named anything, so every
/// open re-verified the same stranded packs from scratch.
#[test]
fn a_push_names_the_packs_it_adopted_in_one_new_index_file() {
    let dir = tempfile::tempdir().unwrap();
    let remote = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let repo = dir.path().join("remote-repo");
    let stage = write_stage(dir.path(), "machine-a", 3, 300_000);
    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();

    seeding_push(&remote, &stage, &mk);
    let stranded = strand_packs(&repo);

    let retry = BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    // The retry re-uploaded nothing of the payload: adoption happened here,
    // before anything was named on disk.
    assert!(
        retry.data_added < 330_000,
        "the retry added {} bytes, which is not dedup against the stranded packs",
        retry.data_added
    );

    // One new index file names every stranded pack exactly once...
    let counts = naming_files(&remote, &mk, &stranded);
    for (id, count) in &counts {
        assert_eq!(
            count, &1,
            "pack {id} is named by {count} index file(s), not one"
        );
    }
    // ...and a fresh survey sees nothing unindexed: the naming is what stops
    // the next open from paying the re-read.
    let (unindexed, _index_files) = survey_unindexed(&remote, &mk);
    assert!(
        unindexed.is_empty(),
        "the survey still finds unindexed packs: {unindexed:?}"
    );
}

/// The second half of the same property: over a repository that is already
/// named, one more push names nothing new and writes no naming file. The
/// pre-write survey is the idempotency, not the push's memory of what it did —
/// which is what two machines need, since neither can see what the other
/// adopted in memory. Red before W1066 for the same reason as the test above.
#[test]
fn a_push_over_named_packs_writes_no_naming_file() {
    let dir = tempfile::tempdir().unwrap();
    let remote = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let repo = dir.path().join("remote-repo");
    let stage = write_stage(dir.path(), "machine-a", 3, 300_000);
    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();

    seeding_push(&remote, &stage, &mk);
    let stranded = strand_packs(&repo);

    let adopting = BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    assert!(
        adopting.data_added < 330_000,
        "the adopting retry re-uploaded {} bytes — the fixture is broken",
        adopting.data_added
    );
    let (unindexed, files_after_adopting) = survey_unindexed(&remote, &mk);
    assert!(unindexed.is_empty());
    let counts = naming_files(&remote, &mk, &stranded);
    assert!(
        counts.values().all(|count| *count == 1),
        "a pack came to be named twice before any rerun: {counts:?}"
    );

    // The rerun: a plain healthy push. It writes its snapshot, and it must
    // add no naming file — the survey finds nothing to name.
    let third = BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    assert!(
        third.data_added < 330_000,
        "a push over named packs re-uploaded {} bytes of data",
        third.data_added
    );
    let (unindexed, files_after_rerun) = survey_unindexed(&remote, &mk);
    assert!(
        unindexed.is_empty(),
        "the rerun found or created stranded packs: {unindexed:?}"
    );
    assert_eq!(
        files_after_rerun, files_after_adopting,
        "the rerun wrote an index file where there was nothing to name"
    );
    let counts = naming_files(&remote, &mk, &stranded);
    assert!(
        counts.values().all(|count| *count == 1),
        "the rerun named a pack a second time: {counts:?}"
    );
}

// ------------------------------------------------- the torn exclusion

/// Torn debris does not refuse the complete packs around it: they are adopted
/// and named, the debris is left exactly as found and named by no index file.
/// Red before W1066: the debris vetoed the whole adoption, so every pack
/// stayed unindexed and the payload went up again.
#[test]
fn torn_debris_does_not_refuse_the_complete_packs_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let remote = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let repo = dir.path().join("remote-repo");
    let stage = write_stage(dir.path(), "machine-a", 3, 300_000);
    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();

    seeding_push(&remote, &stage, &mk);
    let stranded = strand_packs(&repo);
    assert!(
        stranded.len() >= 2,
        "the fixture needs a victim and a survivor"
    );

    // Torn bytes: the smallest pack, half of it — the state a killed writer
    // leaves under a final name (ADR-063's publication no longer produces
    // this; the historical torn files are exactly this shape).
    let (victim, _victim_bytes, victim_path) = smallest_pack(&repo);
    let full = fs::read(&victim_path).unwrap();
    fs::write(&victim_path, &full[..full.len() / 2]).unwrap();
    let torn = fs::read(&victim_path).unwrap();
    let complete: BTreeSet<String> = stranded
        .iter()
        .filter(|id| *id != &victim)
        .cloned()
        .collect();
    assert!(
        !complete.is_empty(),
        "the fixture needs at least one survivor"
    );

    let retry = BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    // The survivors were reused: only the blobs that lived in the torn
    // pack went up again, and a later assertion makes that measurable in
    // names rather than bytes — the survey below is the claim this test
    // pins, because a vetoed adoption leaves every pack unindexed while a
    // debris-excluding one names the survivors.
    assert!(
        retry.data_added < 330_000,
        "the retry re-added {} bytes, so the complete packs beside the debris \
         were not reused",
        retry.data_added
    );
    // The debris itself is untouched.
    assert_eq!(
        fs::read(&victim_path).unwrap(),
        torn,
        "the torn pack must not be modified"
    );

    // Only the debris is still unindexed; the complete packs are named, once.
    let (unindexed, _) = survey_unindexed(&remote, &mk);
    assert_eq!(
        unindexed,
        BTreeSet::from([victim.clone()]),
        "the complete packs should be named; the debris alone stays unindexed"
    );
    let counts = naming_files(&remote, &mk, &complete);
    for (id, count) in &counts {
        assert_eq!(count, &1, "pack {id} is named by {count} index file(s)");
    }
}

/// What the exclusion must not buy: a complete pack whose bytes do not read
/// back as the id it is stored under still refuses the whole adoption —
/// the complete packs included — and nothing is named on disk. Holds before
/// and after W1066; kept so the debris exclusion cannot quietly become a
/// relaxation of the check on packs that could be content.
#[test]
fn a_damaged_complete_pack_still_refuses_the_whole_adoption() {
    let dir = tempfile::tempdir().unwrap();
    let remote = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let repo = dir.path().join("remote-repo");
    let stage = write_stage(dir.path(), "machine-a", 3, 300_000);
    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();

    seeding_push(&remote, &stage, &mk);
    let stranded = strand_packs(&repo);

    // Damage one byte well inside a complete pack's blob ciphertext, leaving
    // header, length and name untouched: not torn — the header still reads
    // and fits — and not verifiable, which is the partial-verification
    // refusal that must stay.
    let (_, _, victim_path) = smallest_pack(&repo);
    let mut bytes = fs::read(&victim_path).unwrap();
    assert!(bytes.len() > 128, "the pack is too small to damage safely");
    bytes[64] ^= 0xff;
    fs::write(&victim_path, &bytes).unwrap();

    let retry = BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    // The refused adoption re-uploaded the payload the damaged pack held.
    assert!(
        retry.data_added >= 450_000,
        "the retry re-added only {} bytes over a damaged complete pack, so it \
         reused bytes it cannot have verified",
        retry.data_added
    );
    let (unindexed, _) = survey_unindexed(&remote, &mk);
    assert_eq!(
        unindexed, stranded,
        "a refused adoption leaves every pack unindexed"
    );
    let counts = naming_files(&remote, &mk, &stranded);
    assert!(
        counts.values().all(|count| *count == 0),
        "a refused adoption named packs on disk: {counts:?}"
    );
}

// ------------------------------------------------------- two clients

/// A spawned-client sandbox for the two-push window: one repository, two
/// `push` processes, both parked so that each one has surveyed what the other
/// has not yet written. The naming and environment plumbing is the w245
/// sandbox's, trimmed to what this test needs.
struct TwoClientSandbox {
    dir: tempfile::TempDir,
    registry: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
}

impl TwoClientSandbox {
    fn new(dir: tempfile::TempDir) -> Self {
        let root = dir.path().to_path_buf();
        for sub in ["home", "config", "data", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W1066 two clients","harnesses":[]}"#,
        )
        .unwrap();
        let stage = root.join("stage");
        let machine = "mbp-two-clients";
        for i in 0..2usize {
            let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-9{i:011}");
            let body = format!(
                r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1","timestamp":"2025-06-01T10:00:00Z"}}"#,
                filler(200_000, 0x9E37_79B9_7F4A_7C15 ^ (i as u64) * 0x517C_C1B7)
            ) + "\n";
            let dir = stage
                .join("sessions")
                .join(machine)
                .join(&session)
                .join("000");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("000001.jsonl"), &body).unwrap();
        }
        Self {
            dir,
            registry,
            repo: root.join("repo"),
            key: root.join("key.json"),
            stage,
        }
    }

    fn command(&self) -> Command {
        let root = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&root.join("home")),
            )
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("rh-cache"))
            .env("CHAT_STASHER_REGISTRY", &self.registry);
        cmd
    }

    fn push_args(&self) -> Vec<String> {
        vec![
            "push".into(),
            "--stage".into(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".into(),
            "mbp-two-clients".into(),
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]
    }

    fn push(&self) -> Output {
        self.command().args(self.push_args()).output().unwrap()
    }

    /// Spawn a `push` that will park at the persist window: its pre-write
    /// survey has run, its naming file has not. `rendezvous` names the
    /// directory the child parks in.
    fn spawn_parked_persist(&self, rendezvous: &Path) -> Child {
        self.command()
            .env("CHAT_STASHER_TEST_HOLD_PERSIST_BEFORE_WRITE", rendezvous)
            .args(self.push_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

/// Whether one child reached its persist rendezvous, tolerating a child that
/// never parks (the run against the unfixed code exits long before any
/// persist hold).
fn reached_or_exited(rendezvous: &Path, child: &mut Child) -> bool {
    let reached = rendezvous.join("reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() {
        if let Ok(Some(_)) = child.try_wait() {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "a push neither reached its persist rendezvous nor exited"
        );
        sleep(Duration::from_millis(2));
    }
    true
}

/// Both clients parked between their pre-write survey and their write is the
/// interleaving no lock prevents, so it is the one the gate has to hold. Both
/// persist; both naming files carry identical entries for the same packs; and
/// the repository they leave behind neither reads wrong nor double-counts:
/// the next push uploads nothing and reports nothing stranded. Red before
/// W1066: neither push named anything, so the survey still finds every pack
/// unindexed.
#[test]
fn two_pushes_that_both_persist_the_same_packs_stay_mergeable() {
    let dir = tempfile::tempdir().unwrap();
    let sb = TwoClientSandbox::new(dir);
    let seeded = sb.push();
    assert!(
        seeded.status.success(),
        "the seeding push failed: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let stranded = strand_packs(&sb.repo);

    let root = sb.dir.path().to_path_buf();
    let rendezvous_a = root.join("persist-park-a");
    let rendezvous_b = root.join("persist-park-b");
    let mut child_a = sb.spawn_parked_persist(&rendezvous_a);
    let mut child_b = sb.spawn_parked_persist(&rendezvous_b);

    let parked_a = reached_or_exited(&rendezvous_a, &mut child_a);
    let parked_b = reached_or_exited(&rendezvous_b, &mut child_b);
    let mut both = Vec::new();
    if parked_a && parked_b {
        // Both surveys are done; neither has written. Release them together:
        // whatever happens next, both naming files land.
        fs::write(rendezvous_a.join("go"), b"").unwrap();
        fs::write(rendezvous_b.join("go"), b"").unwrap();
        both.push(child_a.wait_with_output().unwrap());
        both.push(child_b.wait_with_output().unwrap());
        for out in &both {
            assert!(
                out.status.success(),
                "a parked push failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(
                stdout.contains("persisted - a new index file"),
                "a push that wrote a naming file must say so: {stdout}"
            );
        }
    } else {
        // The unfixed code does not park, so one child finished without
        // reaching the hold; collect them and fall through to the
        // repository-level assertions, which are the red ones.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "On the unparked path the children have already run to completion, and reaping their exit without inspecting their output is all this fallback owes them."
        )]
        let _ = child_a.wait_with_output();
        #[allow(
            clippy::let_underscore_must_use,
            reason = "As above: the assertions this path cares to make are about the repository, not the children."
        )]
        let _ = child_b.wait_with_output();
    }

    // The repository two clients persisted into agrees with itself: nothing
    // is unindexed, the next push claims nothing stranded and uploads nothing,
    // and each stranded pack is named by one or two identical-entry files —
    // never zero, never more than the two clients that could have written.
    let cfg = StoreConfig {
        repo_root: sb.repo.to_string_lossy().into_owned(),
        key_file: sb.key.clone(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: true,
    };
    let key_text = fs::read_to_string(&sb.key).unwrap();
    let mk: MasterKey = serde_json::from_str(&key_text).unwrap();

    let third = sb.push();
    assert!(
        third.status.success(),
        "a push over a doubly-named repository failed: {}",
        String::from_utf8_lossy(&third.stderr)
    );
    let stdout = String::from_utf8_lossy(&third.stdout).to_string();
    assert!(
        stdout
            .lines()
            .all(|line| !line.contains("stranded packs :")),
        "a push over a fully named repository reported stranded packs: {stdout}"
    );
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "a push over a doubly-named repository uploaded data blobs: {stdout}"
    );
    let (unindexed, _) = survey_unindexed(&cfg, &mk);
    assert!(
        unindexed.is_empty(),
        "the survey still finds unindexed packs: {unindexed:?}"
    );
    let counts = naming_files(&cfg, &mk, &stranded);
    for (id, count) in &counts {
        assert!(
            *count >= 1,
            "pack {id} is named by no index file: {counts:?}"
        );
        assert!(
            *count <= 2,
            "pack {id} is named by {count} index files: two clients cannot have written more"
        );
    }
}

/// The `field=value` pairs of the `[push] summary:` line.
fn summary_field(stdout: &str, field: &str) -> Option<u64> {
    let line = stdout.lines().find(|l| l.contains("summary:"))?;
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{field}=")))
        .and_then(|value| value.parse().ok())
}
