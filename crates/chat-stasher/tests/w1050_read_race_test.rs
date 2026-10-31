//! W1050 / READ-RACE — a read-back that lists a snapshot whose tree packs
//! the index its own open does not hold.
//!
//! The observed shape, reproduced as a fixture rather than as a race: a
//! push commits its packs and its index file, then its snapshot file, and a
//! reader that opens between the two still lists the new snapshot
//! afterwards — so the walk stops at `Tree ID … not found in index` even
//! though every byte that snapshot needs is already on disk. The fixture
//! builds exactly that state without any concurrent writer: it pushes
//! twice, then **sidelines** the second push's data pack(s) and index file
//! — moving them aside, headless of the snapshot list — while the second
//! push's snapshot file stays listed. Any reader whose open misses those
//! files reproduces the observation bit for bit.
//!
//! What the approved READ-RACE handling promises (A→C, B not built),
//! asserted here at CLI level on the `read --session` path the read-back
//! drivers use:
//!
//! * **A** — the read waits once (bounded), re-opens the repository — the
//!   index reload — and reads again once; a repository whose sidelined
//!   files are back by then answers, with the body the whole archive
//!   holds. The rendezvous that restores them *between* the two attempts
//!   is `CHAT_STASHER_TEST_HOLD_INDEX_RACE_WAIT`: the child's gate writes
//!   `reached` when it is about to park and polls for `go`, which is the
//!   one window an outside process cannot reach any other way — and why
//!   this fixture does not rely on a 100 ms race it could lose.
//! * **C** — a repository whose files are still missing after the one
//!   retry fails the read: exit 3 ("did not finish"), the pinned reader's
//!   index-miss text, and the walk's own `UNKNOWN:` wording — and never
//!   the proof-of-absence wording (`holds no shards … all of them were
//!   read`), which stays reserved for a walk that read every snapshot and
//!   found nothing.
//! * absence stays provable — a session no snapshot of a healthy
//!   repository holds reads every snapshot and says so in that reserved
//!   wording, distinct from both the raced read's unknown and a match, so
//!   the three states cannot collapse into one another.
//! * **B is not here**: no expected output claims a body came from an
//!   older snapshot in place of the unread newest one, and no flag exists
//!   to make any of that happen.
//!
//! Everything runs on synthetic bytes under temp directories against the
//! local backend; no harness store is read, no network is touched, and the
//! assertions are names, counts, sizes and digests only.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "mbp-read-race";
const SESSION: &str = "claude-code.mbpir.0001-raced-read-race";
const WAIT_MS_ENV: &str = "CHAT_STASHER_TEST_INDEX_RACE_WAIT_MS";
const WAIT_DIR_ENV: &str = "CHAT_STASHER_TEST_HOLD_INDEX_RACE_WAIT";

fn first_shard_bytes() -> Vec<u8> {
    line_bytes(
        "{\"parentUuid\":null,\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\
         \"synthetic read-race filler one\"},\"uuid\":\"u1\",\"timestamp\":\"2026-10-01T00:00:00Z\"}"
            .to_string(),
    )
}

/// The second shard carries visibly different bytes: a recovered read must
/// list both shards, never one standing in for both.
fn second_shard_bytes() -> Vec<u8> {
    line_bytes(format!(
        "{{\"parentUuid\":null,\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\
         \"synthetic read-race filler two {padding}\"}}}}",
        padding = "x".repeat(4096)
    ))
}

fn line_bytes(line: String) -> Vec<u8> {
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every file under a directory, as paths relative to it, sorted.
fn relative_files(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_relative(root, root, &mut out);
    out
}

fn collect_relative(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_dir() {
            collect_relative(root, &path, out);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        out.insert(relative.to_string_lossy().into_owned());
    }
}

/// A suffix that keeps parallel fixtures out of one another's temp roots.
fn unique_suffix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // reason: the suffix only spreads parallel fixtures apart; a
        // nanosecond clock truncated to bits is enough spread and no secret.
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(1)
}

struct Sandbox {
    dir: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
    sidelined: PathBuf,
}

impl Sandbox {
    /// One session, one shard, pushed once: the parent snapshot every later
    /// state is measured against.
    fn new() -> Sandbox {
        let dir = std::env::temp_dir().join(format!("w1050-read-race-{}", unique_suffix()));
        for sub in ["home", "config", "data", "state"] {
            fs::create_dir_all(dir.join(sub)).expect("create the sandbox homes");
        }
        fs::write(
            dir.join("registry.json"),
            r#"{"schema_version":1,"generated":"W1050 read race","harnesses":[]}"#,
        )
        .expect("write the synthetic registry");
        let stage = dir.join("stage");
        let session_dir = stage
            .join("sessions")
            .join(MACHINE)
            .join(SESSION)
            .join("000");
        fs::create_dir_all(&session_dir).expect("create the session's stage directory");
        fs::write(session_dir.join("000001.jsonl"), first_shard_bytes())
            .expect("seed the first shard");
        let sandbox = Sandbox {
            repo: dir.join("repo"),
            key: dir.join("key.json"),
            stage,
            sidelined: dir.join("sidelined"),
            dir,
        };
        sandbox
            .push()
            .expect("the fixture's first push must succeed");
        sandbox
    }

    fn command(&self) -> Command {
        let root = &self.dir;
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
            .env("XDG_CACHE_HOME", root.join("rh-cache"));
        cmd
    }

    fn run(&self, args: &[String]) -> std::io::Result<std::process::Output> {
        self.command().args(args).output()
    }

    /// `push --stage … --machine …` against this sandbox's repository.
    fn push(&self) -> std::io::Result<std::process::Output> {
        self.run(&[
            "push".into(),
            "--stage".into(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".into(),
            MACHINE.into(),
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ])
    }

    /// `read --machine … --session …` against this sandbox's repository.
    fn read_args(&self, session: &str) -> Vec<String> {
        vec![
            "read".into(),
            "--machine".into(),
            MACHINE.into(),
            "--session".into(),
            session.into(),
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]
    }

    /// Push a second time over a planted second shard, then sideline the
    /// second push's pack(s) and index file — everything except its
    /// snapshot file, which stays listed. The repository is then in the
    /// READ-RACE state: the snapshot list names a snapshot the on-disk
    /// index does not resolve.
    fn push_race_lands(&self, who: &str) {
        let before = relative_files(&self.repo);
        assert!(
            before.iter().any(|path| path.starts_with("snapshots/")),
            "{who}: the fixture must have a parent snapshot before the second push"
        );
        let session_dir = self
            .stage
            .join("sessions")
            .join(MACHINE)
            .join(SESSION)
            .join("000");
        fs::write(session_dir.join("000002.jsonl"), second_shard_bytes())
            .expect("plant the second shard");
        let output = self.push().expect("run the second push");
        assert!(
            output.status.success(),
            "{who}: the second push must succeed, so the race state is the push's \
             own commit order rather than a half-written push"
        );
        let after = relative_files(&self.repo);
        let added: Vec<String> = after.difference(&before).cloned().collect();
        let snapshot_files = added
            .iter()
            .filter(|path| path.starts_with("snapshots/"))
            .count();
        let index_files = added
            .iter()
            .filter(|path| path.starts_with("index/"))
            .count();
        let pack_files = added
            .iter()
            .filter(|path| path.starts_with("data/"))
            .count();
        assert_eq!(
            snapshot_files, 1,
            "{who}: the second push adds exactly one snapshot file"
        );
        assert_eq!(
            index_files, 1,
            "{who}: the second push adds exactly one index file"
        );
        assert!(
            pack_files >= 1,
            "{who}: the second push adds at least one pack, because the only \
             already-present bytes are the parent's whole first shard"
        );
        for path in &added {
            if path.starts_with("snapshots/") {
                continue;
            }
            let source = self.repo.join(path);
            let target = self.sidelined.join(path);
            fs::create_dir_all(target.parent().expect("target has a parent"))
                .expect("create the sideline directory");
            fs::rename(&source, &target).expect("move the push's file aside");
        }
    }

    /// Move the sidelined files back, returning how many were restored.
    fn restore_sidelined(&self) -> usize {
        let mut restored = 0;
        for path in relative_files(&self.sidelined) {
            let source = self.sidelined.join(&path);
            let target = self.repo.join(&path);
            fs::create_dir_all(target.parent().expect("target has a parent"))
                .expect("create the repository directory");
            fs::rename(&source, &target).expect("restore the sidelined file");
            restored += 1;
        }
        restored
    }
}

/// Spawn `read --session` and let `between_attempts` run inside the child's
/// bounded wait: the child's READ-RACE gate writes `reached` the moment it
/// parks between its two attempts, so whatever the closure does lands
/// precisely between the raced first attempt and the one re-read — the only
/// window an outside process can reach.
fn read_raced_child<I>(
    sandbox: &Sandbox,
    session: &str,
    between_attempts: I,
) -> (i32, String, String)
where
    I: FnOnce(),
{
    let rendezvous = sandbox.dir.join(format!("rendezvous-{}", unique_suffix()));
    fs::create_dir_all(&rendezvous).expect("create the rendezvous root");
    let mut cmd = sandbox.command();
    cmd.env(WAIT_MS_ENV, "60000")
        .env(WAIT_DIR_ENV, &rendezvous)
        .args(sandbox.read_args(session))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child: Child = cmd.spawn().expect("spawn the read child");
    let reached = rendezvous.join("reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() {
        assert!(
            Instant::now() < deadline,
            "the child never reached its READ-RACE wait; a read that resolved \
             everything on its first attempt would finish, and the raced fixture \
             cannot be tested by waiting"
        );
        sleep(Duration::from_millis(2));
    }
    between_attempts();
    fs::write(rendezvous.join("go"), b"").expect("release the child's wait");
    let output = child.wait_with_output().expect("collect the read child");
    (
        output
            .status
            .code()
            .expect("the child must exit with a code"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The READ-RACE gate's three faces: the reload recovers, the still-missing
/// case stays unknown, and absence stays provable — none of them dressed as
/// another.
#[test]
fn read_race_reload_recovers_and_keeps_unknown() {
    let first_shard = first_shard_bytes();
    let second_shard = second_shard_bytes();
    let whole: Vec<u8> = first_shard
        .iter()
        .chain(second_shard.iter())
        .copied()
        .collect();
    let expected_concat = sha256_hex(&whole);

    // --- A: the bounded wait, the index reload, the one re-read. A
    // repository whose sidelined files are back by then answers with the
    // whole body — both shards, both digests, the exact concatenation.
    let recovered = Sandbox::new();
    recovered.push_race_lands("A");
    let restored_in_window: std::cell::Cell<usize> = std::cell::Cell::new(0);
    let (code, stdout, stderr) = read_raced_child(&recovered, SESSION, || {
        restored_in_window.set(recovered.restore_sidelined());
    });
    assert!(
        restored_in_window.get() >= 2,
        "the fixture must restore at least the sidelined index file and pack(s), \
         or the recovery proves nothing"
    );
    assert_eq!(
        code, 0,
        "the reloaded read must succeed, so a raced read recovers:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[read-back]"),
        "the gate must announce itself and its bounded wait on stderr, or \
         operators cannot tell a race from a stall:\n{stderr}"
    );
    assert!(
        stdout.contains("000001.jsonl") && stdout.contains("000002.jsonl"),
        "the recovered read must list both shards:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("sha256={}", sha256_hex(&first_shard))),
        "the first shard's digest must match its staged bytes:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("sha256={}", sha256_hex(&second_shard))),
        "the second shard's digest must match its staged bytes:\n{stdout}"
    );
    assert!(
        stdout.contains(&expected_concat),
        "the concatenation must be exactly the two staged shards in sequence \
         order:\n{stdout}"
    );

    // --- C: a repository whose files are still missing after the one retry
    // stays an unknown. Exit 3, the index-miss text, the walk's own UNKNOWN
    // wording — and none of the evidence of a completed answer, none of a
    // proof-of-absence.
    let still_broken = Sandbox::new();
    still_broken.push_race_lands("C");
    let mut cmd = still_broken.command();
    cmd.env(WAIT_MS_ENV, "1500")
        .args(still_broken.read_args(SESSION));
    let output = cmd
        .output()
        .expect("run the raced read against the broken repository");
    let code = output
        .status
        .code()
        .expect("the read must exit with a code");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        code, 3,
        "a still-raced read is 'did not finish' — exit 3, never a completed \
         failure and never a success:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not found in index"),
        "the root cause must remain the pinned reader's index-miss:\n{stderr}"
    );
    assert!(
        stderr.contains("UNKNOWN: session"),
        "the walk's own unknown wording must lead the failure:\n{stderr}"
    );
    assert!(
        stderr.contains("the index was reloaded once"),
        "the gate must say its one retry is already spent:\n{stderr}"
    );
    assert!(
        !stderr.contains("holds no shards in any of"),
        "a raced read must never borrow the proof-of-absence wording — that \
         phrase is reserved for a walk that read every snapshot and found \
         nothing:\n{stderr}"
    );
    assert!(
        !stdout.contains("000002.jsonl"),
        "no part of a raced read may render as a completed count:\n{stdout}"
    );

    // --- absence stays provable, and distinct: a session no snapshot of a
    // healthy repository holds reads every snapshot and says so in the
    // reserved wording — which is neither the raced read's text nor a match.
    let healthy = Sandbox::new();
    let missing_session = format!("{SESSION}-absent");
    let mut cmd = healthy.command();
    cmd.args(healthy.read_args(&missing_session));
    let output = cmd
        .output()
        .expect("run the absence read against the healthy repository");
    let code = output
        .status
        .code()
        .expect("the read must exit with a code");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        code, 3,
        "the absence-proof reads the whole archive and still does not finish a \
         session body — exit 3 per the CLI's contract:\nstderr:\n{stderr}\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("holds no shards in any of the 1 snapshots"),
        "an absent session must be reported by the reserved proof-of-absence \
         wording — the one the raced read must never reuse:\n{stderr}"
    );
    assert!(
        !stderr.contains("not found in index"),
        "a healthy repository's absence proof has no business naming an index \
         miss:\n{stderr}"
    );
}
