//! W242 item 2 — what an interrupted push costs, and what it must never cost.
//!
//! `M2-PLAN.md:58-66` states the open question as: a push is one
//! snapshot of the whole stage, so a re-run walks the stage again, and the part
//! already in the repository is supposed to be deduplicated by rustic
//! (`files_unmodified`) — on the condition that the previous index reached the
//! disk, which was never measured. It also notes that because this project never
//! prunes, whatever was uploaded and stranded stays.
//!
//! Measured here, and the premise is wrong in the direction that costs:
//!
//! * rustic accumulates the backup and writes the repository near the end.
//!   Traced at 2 ms resolution: `config` lands at 0.043 s, then nothing at all
//!   until the packs appear at 2.292 s, the index at 2.325 s and the snapshot
//!   at 2.333 s. So the window in which packs are in the repository but no
//!   index references them is a few tens of milliseconds wide locally (it is
//!   the pack-write time, so it scales with the payload and with the network on
//!   a remote destination).
//! * A kill inside that window leaves the packs stranded, and they can never be
//!   reused: every stored object's id covers bytes carrying a fresh random
//!   AEAD nonce, so a later push of the same plaintext produces **different**
//!   bytes and therefore a different id. The re-run reported
//!   `files_unmodified=0` and re-uploaded the whole payload every time.
//! * Killing after the index and snapshot have landed costs nothing — the
//!   retry reports `files_unmodified=<n>` and `data_added=0`.
//!
//! A 560 MB payload was used to measure the quantities, because the window has
//! to be hit deliberately:
//!
//! | kill point | stranded bytes | re-run uploaded | final repository |
//! |---|---|---|---|
//! | 1 pack finalized | 33.6 MB | 560 MB (100 %) | 454 MB, read 300/300, verify OK |
//! | 5 packs finalized | 171.3 MB | 560 MB (100 %) | 592 MB, read 300/300, verify OK |
//! | 10 packs finalized | 343.0 MB | 560 MB (100 %) | 764 MB, read 300/300, verify OK |
//!
//! The permanent cost is therefore bounded by "everything uploaded before the
//! index was written", i.e. up to the whole packed payload minus one pack, and
//! it is never reclaimed — there is no `prune` subcommand (ADR-016 Decision 4
//! took that as a proposal; the CLI confirms it), and an orphan cannot be
//! deduplicated against because ids are not reproducible.
//!
//! What the tests below pin, at sizes that keep the gate fast:
//!
//! * the **safety** property, which holds wherever the kill lands — an
//!   interrupted push never leaves a repository that hands back bytes
//!   differing from what was staged, and the retry restores full coverage;
//! * the **cost** property — a retry after an interrupted push reuses nothing
//!   from the partial upload and re-sends the whole payload.
//!
//! The cost test is written to state the measurement, not to bless it: it is a
//! defect that cannot be fixed inside this crate (the index is written by
//! `rustic_core` at the end of a backup), and it is recorded here so that the
//! `files_unmodified` premise is never silently re-adopted.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// A deterministic, poorly-compressible payload so a push takes measurable
/// time without the test needing a random-number dependency.
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Sandbox {
    dir: tempfile::TempDir,
    registry: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
    /// session id -> the exact shard bytes written for it.
    staged: BTreeMap<String, Vec<u8>>,
}

impl Sandbox {
    /// `sessions` sessions of `bytes_each` bytes each, each shard's bytes known.
    fn new(sessions: usize, bytes_each: usize) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for sub in ["home", "config", "data", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W242 interrupted push","harnesses":[]}"#,
        )
        .unwrap();

        let stage = root.join("stage");
        let machine = "mbp-interrupted";
        let mut staged = BTreeMap::new();
        for i in 0..sessions {
            let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-{i:012}");
            let body = format!(
                r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1","timestamp":"2025-06-01T10:00:00Z"}}"#,
                filler(bytes_each, 0x9E37_79B9_7F4A_7C15 ^ i as u64)
            ) + "\n";
            let dir = stage
                .join("sessions")
                .join(machine)
                .join(&session)
                .join("000");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("000001.jsonl"), &body).unwrap();
            staged.insert(session, body.into_bytes());
        }
        Sandbox {
            dir,
            registry,
            repo: root.join("repo"),
            key: root.join("key.json"),
            stage,
            staged,
        }
    }

    fn command(&self) -> Command {
        let root = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("CHAT_STASHER_REGISTRY", &self.registry);
        cmd
    }

    fn push_args(&self) -> Vec<String> {
        vec![
            "push".into(),
            "--stage".into(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".into(),
            "mbp-interrupted".into(),
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]
    }

    fn spawn_push(&self) -> Child {
        self.command()
            .args(self.push_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn push(&self) -> Output {
        self.command().args(self.push_args()).output().unwrap()
    }

    fn read_archive(&self) -> Output {
        self.command()
            .args([
                "read",
                "--all-machines",
                "--full-ids",
                "--repo",
                self.repo.to_str().unwrap(),
                "--key-file",
                self.key.to_str().unwrap(),
                "--keep-ssh-masters",
            ])
            .output()
            .unwrap()
    }
}

/// `    session <full id> shards=N   bytes=N      sha256=<hex>`
fn parse_session_shas(stdout: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("session ") else {
            continue;
        };
        let Some(id) = rest.split_whitespace().next() else {
            continue;
        };
        if let Some(sha) = rest
            .split_whitespace()
            .find_map(|token| token.strip_prefix("sha256="))
        {
            found.insert(id.to_string(), sha.to_string());
        }
    }
    found
}

/// Read the `key=value` pairs out of the `[push] summary:` line.
fn summary_field(stdout: &str, field: &str) -> Option<u64> {
    let line = stdout.lines().find(|l| l.contains("summary:"))?;
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{field}=")))
        .and_then(|value| value.parse().ok())
}

/// Whatever `read` hands back must equal what was staged, session by session.
/// The criterion ADR-016 Decision 4 states for the concurrency measurement —
/// whether any file ever came back with a hash that does not match — applied
/// here to the interrupted case.
fn assert_no_mismatched_content(sb: &Sandbox, context: &str) -> usize {
    let out = sb.read_archive();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let archived = parse_session_shas(&stdout);
    for (session, sha) in &archived {
        let staged = sb
            .staged
            .get(session)
            .unwrap_or_else(|| panic!("{context}: the archive holds an unknown session {session}"));
        assert_eq!(
            sha,
            &sha256_hex(staged),
            "{context}: {session} came back with bytes that differ from the stage"
        );
    }
    archived.len()
}

fn wait_for_repo_config(repo: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if repo.join("config").exists() {
            return;
        }
        sleep(Duration::from_millis(2));
    }
    panic!("the repository was never initialised within {timeout:?}");
}

/// Kill a push at a chosen moment, then assert the archive is still honest and
/// the retry restores full coverage. Repeated at three kill points, all after
/// the repository exists so each one interrupts a push that had started work.
///
/// The delays are chosen against the measured push duration for this payload
/// (~1.3 s in a debug build), and the test fails loudly if the push ever
/// finishes before the kill lands — a silent no-op would be worse than a slow
/// test.
#[test]
fn an_interrupted_push_never_leaves_a_corrupt_archive() {
    for (label, delay) in [
        ("at-init", Duration::ZERO),
        ("mid-backup", Duration::from_millis(100)),
        ("late", Duration::from_millis(800)),
    ] {
        let sb = Sandbox::new(12, 2_000_000);
        let mut child = sb.spawn_push();
        wait_for_repo_config(&sb.repo, Duration::from_secs(30));
        sleep(delay);
        child.kill().unwrap_or_else(|err| {
            panic!(
                "{label}: the push finished before the kill landed ({err}); enlarge the payload \
                 so this case keeps interrupting a running push"
            )
        });
        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "{label}: a killed push must not report success"
        );

        // Nothing the archive hands back may differ from the stage.
        assert_no_mismatched_content(&sb, label);

        // The stage is the source of truth and a failed push may not touch it.
        for (session, body) in &sb.staged {
            let shard = sb
                .stage
                .join("sessions/mbp-interrupted")
                .join(session)
                .join("000/000001.jsonl");
            assert_eq!(
                &fs::read(&shard).unwrap(),
                body,
                "{label}: the interrupted push modified the stage"
            );
        }

        // The retry archives everything, byte-perfect.
        let retry = sb.push();
        assert!(
            retry.status.success(),
            "{label}: the retry failed: {}",
            String::from_utf8_lossy(&retry.stderr)
        );
        let count = assert_no_mismatched_content(&sb, &format!("{label} after retry"));
        assert_eq!(
            count,
            sb.staged.len(),
            "{label}: the retry did not archive every staged session"
        );
    }
}

/// The other half of the M2-PLAN premise, and the contrast that makes the cost
/// test below readable: when the index *did* reach the disk, the next push of
/// the same stage is a genuine no-op — dedup works. It is only the interrupted
/// case that cannot reuse anything.
#[test]
fn a_completed_push_makes_the_next_push_a_no_op() {
    let sb = Sandbox::new(12, 2_000_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = sb.push();
    assert!(second.status.success());
    let stdout = String::from_utf8_lossy(&second.stdout);
    let unmodified = summary_field(&stdout, "files_unmodified")
        .unwrap_or_else(|| panic!("no files_unmodified in the summary: {stdout}"));
    assert!(
        unmodified >= sb.staged.len() as u64,
        "a second push of an unchanged stage must report every staged file unmodified, \
         got {unmodified}: {stdout}"
    );
    assert_eq!(
        summary_field(&stdout, "data_added"),
        Some(0),
        "a second push of an unchanged stage must upload no data: {stdout}"
    );
}

/// The measured cost of an interruption: the retry cannot reuse anything the
/// interrupted run left behind, because the interrupted run left nothing the
/// retry can see.
///
/// The kill lands at the moment the repository `config` appears — the earliest
/// point at which a push has written anything at all, and unambiguous as a
/// "the push did not finish" signal, since `config` is written during init and
/// the backup has not started. The assertion is on the archive, not on
/// `data_added`: that field is rustic's own accounting and is not the number of
/// bytes uploaded (measured: 20.3 MB for a 24.0 MB payload that nothing could
/// have deduplicated against, because the repository held only `config`).
#[test]
fn a_retry_after_an_interrupted_push_reuses_nothing_from_the_partial_upload() {
    let sb = Sandbox::new(12, 2_000_000);

    let mut child = sb.spawn_push();
    wait_for_repo_config(&sb.repo, Duration::from_secs(30));
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());

    // The interrupted run's work is invisible: no snapshot, so nothing for a
    // retry to be "unmodified" against.
    assert!(
        parse_session_shas(&String::from_utf8_lossy(&sb.read_archive().stdout)).is_empty(),
        "a push killed during init must leave no snapshot behind"
    );

    let retry = sb.push();
    assert!(
        retry.status.success(),
        "the retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let stdout = String::from_utf8_lossy(&retry.stdout);
    assert_eq!(
        summary_field(&stdout, "files_unmodified"),
        Some(0),
        "an interrupted push left nothing reusable, so no file may count as unmodified: {stdout}"
    );

    // And the payload really is all there afterwards.
    assert_eq!(
        assert_no_mismatched_content(&sb, "after the retry"),
        sb.staged.len()
    );
}

/// The permanent half of the cost: packs that reached the repository without
/// their index are unreachable, and no later push can ever reclaim them.
///
/// This is why the waste is permanent rather than merely deferred — a stored
/// object's id covers bytes carrying a fresh random AEAD nonce, so re-uploading
/// the same plaintext produces different bytes under a different id and can
/// never match the stranded copy. There is also no `prune` subcommand to remove
/// it with (ADR-016 Decision 4 proposed exactly that, and the CLI confirms it
/// is not published).
///
/// A kill inside the few-tens-of-milliseconds window between "packs finalized"
/// and "index written" leaves precisely this state, which is what the 560 MB
/// probe measured. Reproducing it here at test scale cannot be done by racing
/// that window, so the state is constructed directly: an index and snapshot are
/// removed from a completed repository, leaving complete packs that nothing
/// references. What is asserted is the behaviour of the *next* push against
/// that state, which is the same behaviour a real interrupted push gets.
#[test]
fn packs_stranded_without_an_index_are_never_reclaimed() {
    let sb = Sandbox::new(4, 500_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Strand the packs: drop the index and the snapshot, keep `config` and the
    // packs, exactly the state a kill in the window leaves.
    // Repository-relative paths of every stored object, packs included: packs
    // live two levels down (`data/<xx>/<id>`), so this has to recurse.
    //
    // Components are joined with `/` because the filter below selects packs by
    // the `data/` prefix; `to_string_lossy` alone gives `data\` on Windows,
    // which matches nothing and would leave `stranded` empty.
    let pack_names = |repo: &Path| -> Vec<String> {
        let mut names = Vec::new();
        let mut stack = vec![repo.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(repo) {
                    names.push(
                        rel.components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
        }
        names.sort();
        names
    };
    let after_first = pack_names(&sb.repo);
    let stranded: Vec<&String> = after_first
        .iter()
        .filter(|n| n.starts_with("data/"))
        .collect();
    assert!(
        !stranded.is_empty(),
        "the first push wrote no packs to strand"
    );
    for entry in ["index", "snapshots"] {
        let dir = sb.repo.join(entry);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    let before = repo_bytes(&sb.repo);

    let second = sb.push();
    assert!(
        second.status.success(),
        "the push over a repository with no index failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert_eq!(
        summary_field(&stdout, "files_unmodified"),
        Some(0),
        "nothing may be counted unmodified when the index that referenced it is gone: {stdout}"
    );

    // Every stranded pack is still there — nothing reclaims it.
    let after = pack_names(&sb.repo);
    for name in &stranded {
        assert!(
            after.contains(*name),
            "the stranded pack {name} disappeared; nothing in this design may reclaim it"
        );
    }
    // And the whole payload was uploaded again on top of it.
    let grew = repo_bytes(&sb.repo) - before;
    assert!(
        grew > before / 2,
        "the retry added only {grew} bytes over a stranded {before}-byte repository, which is \
         not a second full copy"
    );

    // Reported, not asserted on: this is the quantity the write-up carries,
    // and a future rustic that started reclaiming packs would show it moving.
    eprintln!(
        "W242: stranded={before} bytes, second push added {grew} bytes; payload was {} bytes",
        sb.staged.values().map(Vec::len).sum::<usize>()
    );

    assert_eq!(
        assert_no_mismatched_content(&sb, "after the second push"),
        sb.staged.len()
    );
}

/// Total bytes of every file below `root`.
fn repo_bytes(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = path.metadata() {
                total += meta.len();
            }
        }
    }
    total
}
