//! ISSUE-006 step A (W1040): debris an interrupted push leaves behind — a
//! half-written pack under its final name — is classified, worded and counted
//! as debris, never as corruption of a backup.
//!
//! The three fixtures the change is gated on, one playground each:
//!
//! * **1 · unreferenced torn pack + a normal indexed pack** — the state the
//!   real first mirror sync hit: every repository-scanning command
//!   (`mirror sync`, `status`, `doctor`, `verify`, `read`, `export`) exits 0,
//!   the torn pack never enters the mirror, `status` reports exactly 1 torn,
//!   the torn counts ride `prune-orphans` as their own count and byte total,
//!   and no user-facing string in any of those outputs says "corrupt".
//! * **2 · the same bad bytes, named by an index** — still exit 1: an index
//!   file naming bytes that do not match is a real backup's corruption, and
//!   the classification must never skip it.
//! * **3 · one complete unindexed pack** — still stranded (adoptable), never
//!   reported as torn: the adopt-on-open behaviour is today's.
//!
//! Everything is local, synthetic and under a sandbox (W306): no session
//! names, no machine identity beyond the invented ones below, no network, and
//! no read of a masterkey the tests do not produce themselves.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "../src/test_support.rs"]
mod test_support;

/// The one sentence every surface prints for torn debris (ISSUE-006 step A),
/// spelled out in the test so a wording change has to notice it here.
const SENTENCE: &str =
    "An interrupted push left a partial file. It belongs to no backup and can be ignored.";

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

fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

/// A two-machine synthetic repository (the standard mirror-side shape) plus
/// the isolation envelope for the spawned binary.
struct Sandbox {
    dir: tempfile::TempDir,
    #[allow(dead_code)]
    mk: MasterKey,
}

impl Sandbox {
    fn repo(&self) -> PathBuf {
        self.dir.path().join("remote-repo")
    }

    fn key(&self) -> PathBuf {
        self.dir.path().join("remote.key")
    }

    fn mirror_root(&self) -> PathBuf {
        self.dir
            .path()
            .join("data")
            .join("chat-stasher")
            .join("mirror")
            .join("storagebox")
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
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    /// A `[mirror]` declaration pointing at this repository — the LM-5 shape.
    fn write_mirror_config(&self) {
        let config_dir = self.dir.path().join("config").join("chat-stasher");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[destinations.storagebox]\nrepo = \"{}\"\nkey_file = \"{}\"\nno_cache = \
                 true\n\n[mirror]\nenabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = \
                 1.0\n",
                self.repo().display(),
                self.key().display()
            ),
        )
        .unwrap();
    }

    /// The empty registry `doctor`'s scanner needs (and no harness probing).
    fn write_registry(&self) {
        fs::write(
            self.dir.path().join("registry.json"),
            r#"{"schema_version":1,"generated":"T1 synthetic","harnesses":[]}"#,
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.env(
            "CHAT_STASHER_REGISTRY",
            self.dir.path().join("registry.json"),
        );
        command.args(args).output().unwrap()
    }
}

fn sandbox_with_repository() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let remote = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    for (stage, machine) in [("stage-a", "machine-a"), ("stage-b", "machine-b")] {
        write_session(&distinctive_stage(&dir, stage), machine, "session-a1", 3);
    }
    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();
    for (stage, machine) in [("stage-a", "machine-a"), ("stage-b", "machine-b")] {
        BackupStore::new(remote.clone(), machine.to_string())
            .push(&distinctive_stage(&dir, stage), &mk)
            .unwrap();
    }
    Sandbox { dir, mk }
}

/// A stage path joined the boring way, per the tuple loop above. Kept as a
/// function so the two loops share one spelling.
fn distinctive_stage(dir: &tempfile::TempDir, stage: &str) -> PathBuf {
    dir.path().join(stage)
}

/// Every final pack file under the repository's `data/`, sorted.
fn pack_paths(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo.join("data")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// The torn bytes: a final-named pack file whose unencrypted tail length field
/// declares a header hundreds of megabytes long in a byte-sized file — the
/// exact shape the real event left (`data/68/681e95a2…`, 1 966 080 bytes with
/// a 1 697 901 003-byte header declaration) — so the classification decision
/// ("the header does not fit") is deterministic to the byte.
fn torn_bytes(listed_bytes: u64) -> Vec<u8> {
    let mut bytes = vec![0x44u8; usize::try_from(listed_bytes).unwrap().max(4)];
    // The declared header length: far larger than any file this fixture
    // writes, so test-case 2 reuses these bytes under an indexed name and the
    // same denial decides both fixtures.
    let header_len: u32 = 0x6550_0000;
    bytes.truncate(usize::try_from(listed_bytes).unwrap().max(4));
    let last = bytes.len() - 4;
    bytes[last..].copy_from_slice(&header_len.to_le_bytes());
    bytes
}

/// Plant one unreferenced torn pack into the repository, and return its
/// repository-relative `data/xx/<id>` path and its byte size.
fn plant_torn_pack(sandbox: &Sandbox) -> (PathBuf, u64) {
    let id = "ab".to_string() + &"01".repeat(31);
    let relative: PathBuf = ["data", &id[..2], &id].iter().collect();
    let path = sandbox.repo().join(&relative);
    let bytes = torn_bytes(1000);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &bytes).unwrap();
    (relative, bytes.len() as u64)
}

/// The fixture's wording rule: no user-facing string a command prints about
/// the *archive or its debris* may contain "corrupt" (ISSUE-006 step A: the
/// debris must never be called corrupt, damaged, or source corruption).
///
/// One pre-existing label is not a claim about the archive: the body cache's
/// own deficit counter, `corrupt=0`, which `read`/`search` print identically on
/// a pristine machine and which `body_cache.rs` already keeps semantically
/// apart from archive corruption ("this laptop's cache", never "the archive").
/// Its zero spelling is scrubbed before the check — a *non-zero* counter, or
/// the word anywhere else in the output, still reds this test.
fn assert_no_corrupt_claim(output: &str, surface: &str) {
    let claims = output.replace("corrupt=0", "");
    assert!(
        !claims.to_ascii_lowercase().contains("corrupt"),
        "{surface} must not call interrupted-push debris corrupt:\n{output}"
    );
}

/// Run one command, collect stdout+stderr, and assert on exit code and the
/// prose together — the outputs are the claim under test.
fn run_all(sandbox: &Sandbox, args: &[&str]) -> (i32, String) {
    let out = sandbox.run(args);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), all)
}

// ---------------------------------------------------- fixture 1

/// One unreferenced torn pack plus one normal indexed pack: every scanning
/// command exits 0 (a reminder is fine; a failure is not), the torn pack is
/// absent from the mirror, `status` reports exactly 1 torn, and no
/// user-facing string in any of the six outputs contains "corrupt".
#[test]
fn a_torn_pack_is_debris_every_command_skips_and_reports() {
    let sandbox = sandbox_with_repository();
    sandbox.write_mirror_config();
    sandbox.write_registry();
    let (torn_rel, torn_bytes_len) = plant_torn_pack(&sandbox);
    assert_eq!(
        torn_bytes_len, 1000,
        "the fixture's torn pack is 1000 bytes"
    );
    // The fixture's premise: no index file names the torn pack, and the
    // repository also holds ordinary indexed packs (two pushed machines).
    assert!(
        pack_paths(&sandbox.repo()).len() >= 2,
        "the fixture repository holds indexed packs beyond the torn one"
    );

    // --- mirror sync: completes, never copies the debris, never fails.
    let (code, all) = run_all(&sandbox, &["mirror", "sync", "--budget-secs", "120"]);
    assert_eq!(code, 0, "torn debris must not fail the mirror sync:\n{all}");
    assert!(
        all.contains("1 file(s) / 1000 B skipped as interrupted-push debris"),
        "the round states the skip beside its counts:\n{all}"
    );
    assert!(
        all.contains(SENTENCE),
        "the mirror's reminder carries the one sentence:\n{all}"
    );
    assert_no_corrupt_claim(&all, "mirror sync");
    assert!(
        !sandbox.mirror_root().join(&torn_rel).exists(),
        "the torn pack must never be downloaded into the mirror"
    );
    assert!(
        !sandbox
            .mirror_root()
            .join(&torn_rel)
            .with_file_name(format!(
                ".{}.tmp",
                torn_rel.file_name().unwrap().to_string_lossy()
            ))
            .exists(),
        "and no torn-write temp of it is left behind either"
    );

    // --- one real pass, for two reasons: the scheduled pass itself must
    // complete over debris (it ends in a mirror round too), and `status`'s
    // exit code reads the run-state record only such a pass writes — the
    // fixture machine's own health, independent of the debris verdicts.
    let stage_path = sandbox
        .dir
        .path()
        .join("stage-a")
        .to_string_lossy()
        .into_owned();
    let (code, all) = run_all(
        &sandbox,
        &[
            "run-once",
            "--stage",
            &stage_path,
            "--machine",
            "machine-a",
            "--destination",
            "storagebox",
        ],
    );
    assert_eq!(code, 0, "the scheduled pass completes over debris:\n{all}");
    assert!(
        !all.contains("mirror-sync warning"),
        "the pass's mirror step is not a warning over debris:\n{all}"
    );

    // --- status: exactly 1 torn, worded as debris, never as corruption.
    let (code, all) = run_all(&sandbox, &["status"]);
    assert_eq!(
        code, 0,
        "a mirrored machine with debris reports cleanly:\n{all}"
    );
    assert!(
        all.contains("1 torn file(s)"),
        "status reports exactly 1 torn:\n{all}"
    );
    assert!(
        all.contains(SENTENCE),
        "the status row carries the one sentence:\n{all}"
    );
    assert_no_corrupt_claim(&all, "status");

    // --- doctor: D7 counts the debris at the destination, D13 shows the
    // mirror's own row — and the machine's diagnosis stays a success.
    let (code, all) = run_all(&sandbox, &["doctor"]);
    assert_eq!(code, 0, "debris is not a doctor failure:\n{all}");
    assert!(
        all.contains("1 torn pack(s) (1000 B)"),
        "doctor reports the torn count and byte total separately:\n{all}"
    );
    assert!(
        !all.contains("stranded pack(s) ("),
        "sanity: the fixture holds no stranded content"
    );
    assert!(
        all.contains(SENTENCE),
        "doctor's debris line carries the one sentence:\n{all}"
    );
    assert!(
        all.contains("earliest-seen time unknown"),
        "doctor names the earliest-seen time unknown rather than inventing one:\n{all}"
    );
    assert_no_corrupt_claim(&all, "doctor");

    // --- verify: the torn reminder rides the report without being a finding.
    let (code, all) = run_all(
        &sandbox,
        &[
            "verify",
            "--level",
            "l2",
            "--destination",
            "storagebox",
            "--machine",
            "machine-a",
        ],
    );
    assert_eq!(code, 0, "torn debris must not fail verify:\n{all}");
    assert!(
        all.contains("1 torn pack(s) (1000 B)"),
        "verify counts the debris beside its findings:\n{all}"
    );
    assert!(
        all.contains("[verify] RESULT         : OK"),
        "the verdict itself stays OK:\n{all}"
    );
    assert!(
        all.contains(SENTENCE),
        "verify's reminder carries the one sentence:\n{all}"
    );
    assert_no_corrupt_claim(&all, "verify");

    // --- read: the archive is unaffected (its indexes name none of the
    // debris), so an ordinary read answers normally.
    let (code, all) = run_all(
        &sandbox,
        &[
            "read",
            "--destination",
            "storagebox",
            "--machine",
            "machine-a",
            "--session",
            "session-a1",
        ],
    );
    assert_eq!(code, 0, "reads pass over debris they never touch:\n{all}");
    assert_no_corrupt_claim(&all, "read");

    // --- export: same archive, same answer, files written.
    let out_dir = sandbox.dir.path().join("exported");
    let stage_out = out_dir.to_string_lossy().into_owned();
    let (code, all) = run_all(
        &sandbox,
        &["export", "--destination", "storagebox", "--out", &stage_out],
    );
    assert_eq!(code, 0, "export passes over debris it never reads:\n{all}");
    assert_no_corrupt_claim(&all, "export");

    // --- prune-orphans: torn and stranded count apart, torn is not an
    // unknown, and the survey is complete — exit 0, not 3.
    let (code, all) = run_all(&sandbox, &["prune-orphans", "--destination", "storagebox"]);
    assert_eq!(
        code, 0,
        "torn debris is a known state, never an unfinished read:\n{all}"
    );
    assert!(
        all.contains("1 torn (1000 B), 0 stranded (0 B), 0 unknown (0 B)"),
        "the unindexed line splits torn from stranded, with byte totals:\n{all}"
    );
    assert!(
        all.contains(" RESULT      : COMPLETE"),
        "a survey that classified every pack is complete:\n{all}"
    );
    assert!(
        all.contains(SENTENCE),
        "prune-orphans words torn debris with the one sentence:\n{all}"
    );
    assert_no_corrupt_claim(&all, "prune-orphans");

    // --- the same facts in --json, for scripts.
    let (code, all) = run_all(
        &sandbox,
        &["prune-orphans", "--destination", "storagebox", "--json"],
    );
    assert_eq!(code, 0, "the structured survey is also complete:\n{all}");
    let value: serde_json::Value =
        serde_json::from_str(all.trim()).unwrap_or_else(|e| panic!("json: {e}\n{all}"));
    assert_eq!(value["packs"]["torn"], serde_json::json!(1));
    assert_eq!(value["packs"]["torn_bytes"], serde_json::json!(1000));
    assert_eq!(value["packs"]["stranded"], serde_json::json!(0));
    assert_eq!(value["packs"]["unknown"], serde_json::json!(0));
    assert_eq!(value["candidates"][0]["status"], serde_json::json!("torn"));
    assert_eq!(
        value["candidates"][0]["first_seen"]["kind"],
        serde_json::json!("unknown")
    );

    // --- and the mirror covering a second round over the same debris: the
    // classification is re-made from the bytes, and the round stays exit 0
    // ( debris never becomes a permanent hold ).
    let before = fs::read(&sandbox.repo().join(&torn_rel)).unwrap();
    let (code, all) = run_all(&sandbox, &["mirror", "sync", "--budget-secs", "120"]);
    assert_eq!(code, 0, "the second round over the same debris:\n{all}");
    assert!(
        !sandbox.mirror_root().join(&torn_rel).exists(),
        "and it still never copies the debris"
    );
    let after = fs::read(&sandbox.repo().join(&torn_rel)).unwrap();
    assert_eq!(before, after, "nothing may touch the debris remotely");
}

// ---------------------------------------------------- fixture 2

/// The same bad bytes, named by an index: still exit 1. An index file naming
/// bytes that do not match is the checked-and-failed fact it always was —
/// the classification must never widen a skip over real backup content.
#[test]
fn the_same_bad_bytes_named_by_an_index_still_fail_mirror_sync() {
    let sandbox = sandbox_with_repository();
    sandbox.write_mirror_config();
    sandbox.write_registry();

    // Overwrite one *indexed* pack under its own name with the same torn
    // bytes: the listing keeps naming it, the index keeps pointing at it, and
    // the bytes are the fixture-1 debris.
    let packs = pack_paths(&sandbox.repo());
    assert!(!packs.is_empty(), "the fixture repository holds packs");
    let victim = &packs[0];
    let victim_bytes = fs::read(victim).unwrap();
    fs::write(
        victim,
        torn_bytes(u64::try_from(victim_bytes.len()).unwrap()),
    )
    .unwrap();

    let (code, all) = run_all(&sandbox, &["mirror", "sync", "--budget-secs", "120"]);
    assert_eq!(
        code, 1,
        "corruption of a backup an index names is the finished-and-failed class:\n{all}"
    );
    assert!(
        all.contains("exit_code=1 — the source proved corrupt"),
        "the exit-1 class keeps its sentence:\n{all}"
    );
    // And the debris wording never appears: these bytes are not debris.
    assert!(
        !all.contains(SENTENCE),
        "an indexed corrupt pack must not be worded as interrupted-push debris:\n{all}"
    );
}

// ---------------------------------------------------- fixture 3

/// One complete unindexed pack: still stranded — adoptable content, not
/// debris — and no surface calls it torn.
#[test]
fn a_complete_unindexed_pack_is_stranded_and_never_torn() {
    let sandbox = sandbox_with_repository();
    sandbox.write_mirror_config();
    sandbox.write_registry();

    // The killed-push shape (W242's recipe): every pack complete, the index
    // and snapshot files never written. The pushes above wrote several packs;
    // all of them are unindexed but complete once the index files are gone.
    for section in ["index", "snapshots"] {
        let dir = sandbox.repo().join(section);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    let stranded = pack_paths(&sandbox.repo());
    assert!(
        stranded.len() >= 2,
        "the stranding kept every pack and removed the index"
    );

    // prune-orphans: every candidate is stranded (verified), none torn.
    let (code, all) = run_all(&sandbox, &["prune-orphans", "--destination", "storagebox"]);
    assert_eq!(
        code, 0,
        "stranded content is a complete survey, not a failure:\n{all}"
    );
    assert!(
        all.contains(&format!("0 torn (0 B), {} stranded (", stranded.len())),
        "every complete unindexed pack counts as stranded, not torn:\n{all}"
    );
    assert!(
        all.contains("would adopt all"),
        "the next open adopts them, exactly as before:\n{all}"
    );
    for pack in &stranded {
        let prefix = pack.file_name().unwrap().to_string_lossy().into_owned()[..12].to_string();
        let line = all
            .lines()
            .find(|line| line.contains(&prefix))
            .unwrap_or_else(|| panic!("candidate {prefix} missing:\n{all}"));
        assert!(
            line.contains("stranded"),
            "candidate {prefix} words as stranded, not torn:\n{line}"
        );
    }

    // mirror sync over a repository whose listing is all stranded packs:
    // complete packs copy normally (their bytes hash to their own names), the
    // round completes, and no torn word appears.
    let (code, all) = run_all(&sandbox, &["mirror", "sync", "--budget-secs", "120"]);
    assert_eq!(
        code, 0,
        "complete unindexed packs copy like any other file:\n{all}"
    );
    assert!(
        !all.to_ascii_lowercase().contains("torn"),
        "stranded content must never be worded as torn:\n{all}"
    );
}
