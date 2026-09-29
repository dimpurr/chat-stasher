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
//! * Before W245, a kill inside that window left packs stranded and they could
//!   not be reused: every stored object's id covers bytes carrying a fresh
//!   random AEAD nonce, so a later push of the same plaintext produced
//!   **different** bytes and therefore a different id. The re-run reported
//!   `files_unmodified=0` and re-uploaded the whole payload every time.
//! * Killing after the index and snapshot have landed costs nothing — the
//!   retry reports `files_unmodified=<n>` and adds no bytes at all, content or
//!   tree. The content claim is `data_blobs`, not `data_added`, because the
//!   latter also counts *tree* blobs; the tree claim is that it too is zero.
//!   Windows reported `files_unmodified=13 data_blobs=0 data_added=1690` (and
//!   `data_added=1602` for the same push once a stored ctime was dropped): one
//!   tree re-serialized while every file came back unmodified and no content
//!   was written. What moves there is a stored *directory* time — the time of
//!   the writes into the directory, which the filesystem reports lazily and
//!   which no counter in the summary reports at all, since the file counters
//!   cannot see a directory. `store.rs` no longer stores one, and the two
//!   tests below pin the event on any platform. See
//!   `a_completed_push_makes_the_next_push_a_no_op`.
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
//! Before W245, the permanent cost was bounded by "everything uploaded before
//! the index was written", i.e. up to the whole packed payload minus one pack.
//! This test still ensures that packs are never deleted, while W245 adopts their
//! verified contents instead of uploading a second copy.
//!
//! What the tests below pin, at sizes that keep the gate fast:
//!
//! * the **safety** property, which holds wherever the kill lands — an
//!   interrupted push never leaves a repository that hands back bytes
//!   differing from what was staged, and the retry restores full coverage;
//! * the **append-only** property — a retry after an interrupted push leaves
//!   the partial upload's packs on disk and reuses their verified contents.
//!
//! The interrupted-push cost test lives in
//! `w245_interrupted_push_test.rs`, including pack verification and adoption.

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

    /// Field-by-field node metadata difference between this sandbox's two
    /// newest snapshots, for a failing no-op-push assertion to print.
    ///
    /// Diagnostics must not be able to fail the test they report on, so a
    /// problem reading the repository comes back as text: the assertion that
    /// called this is the failure the reader needs, with whatever this could
    /// establish attached to it.
    fn node_diff(&self) -> String {
        let cfg = chat_stasher::store::StoreConfig {
            repo_root: self.repo.to_string_lossy().into_owned(),
            key_file: self.key.clone(),
            ..Default::default()
        };
        let store = chat_stasher::store::BackupStore::new(cfg, "mbp-interrupted".to_string());
        let mk = match fs::read_to_string(&self.key)
            .map_err(anyhow::Error::from)
            .and_then(|raw| chat_stasher::store::parse_key(&raw))
        {
            Ok(mk) => mk,
            Err(e) => return format!("could not load the repository key: {e:#}"),
        };
        store
            .node_metadata_diff(&mk)
            .unwrap_or_else(|e| format!("could not build the node metadata diff: {e:#}"))
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

/// Every file under `root`, depth-first and sorted, so two walks of one tree
/// produce the same list in the same order.
fn stage_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Every directory below `root` (not `root` itself), depth-first and sorted —
/// the same walk as [`stage_files`], listing the entries that carry children
/// instead of the ones that carry bytes.
fn stage_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
            }
        }
    }
    dirs.sort();
    dirs
}

/// Read the `key=value` pairs out of the `[push] summary:` line.
fn summary_field(stdout: &str, field: &str) -> Option<u64> {
    let line = stdout.lines().find(|l| l.contains("summary:"))?;
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{field}=")))
        .and_then(|value| value.parse().ok())
}

/// A push that read no file must also have written no bytes: `data_added`
/// counts *tree* bytes as well as content ones (`data_blobs` counts content
/// alone), so a non-zero `data_added` here is a stored node whose metadata
/// moved between the two pushes.
///
/// The summary cannot say which node or which field, and the platform that
/// moves one is not the platform the gate usually runs on, so the failure
/// carries the whole field-by-field diff of the two snapshots. That diff is
/// the point of this assertion: it is what identifies the field on a host
/// nobody can run by hand — a Windows CI cell, where the answer is only ever
/// visible in a log.
fn assert_no_tree_bytes(sb: &Sandbox, stdout: &str, context: &str) {
    let added = summary_field(stdout, "data_added");
    if added == Some(0) {
        return;
    }
    panic!(
        "{context}: data_added={added:?} (data_blobs={:?}) — a tree was re-serialized.\n\
         push output:\n{stdout}\n\
         {}",
        summary_field(stdout, "data_blobs"),
        sb.node_diff()
    );
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
///
/// **"No-op" means no bytes at all, content or tree.** `data_blobs` is the
/// content claim; `data_added` is the stronger one, because `rustic_core` adds
/// `summary.data_added += self.data` for *both* its packers and
/// `summary.data_blobs += self.blobs` only for the data one (crate
/// `vendor/rustic_core/src/blob/packer.rs`, `PackerStats::apply`), so tree bytes land in
/// `data_added` alone. Tree bytes are not churn to be tolerated here: the nodes
/// in that tree are written by this project, and a field that moves on its own
/// re-serializes the tree on every scheduled push.
///
/// This crate used to tolerate exactly that on Windows, where a second push of
/// an unchanged stage reported `files_unmodified=13 data_blobs=0
/// data_added=1690`: every staged file byte-identical and unre-written, no
/// content blob written, and one tree re-serialized. That number survived the
/// storage policy's first half (a stored `ctime` dropped: 1690 → 1602, the same
/// 1602 in a test whose stage holds a third of the sessions, so the tree is one
/// that does not depend on the shards). Every field a *file* node stores is
/// either compared — and `files_changed=0` says no file differed — or fixed by
/// the mapper, which leaves the field no counter looks at: a *directory*'s
/// time, which is the time of the writes into it and is reported lazily by the
/// platform. `store.rs` now pins the storage side of the metadata policy as
/// well as the comparison side and stores no directory times either, so a no-op
/// push adds nothing on any platform. The Windows cell is where the platform
/// itself is exercised; `a_metadata_field_the_archive_does_not_use_cannot_re_serialize_a_tree`
/// and `a_directory_mtime_that_moves_cannot_re_serialize_a_tree` below
/// reproduce both events on a host that does not have to be Windows. When this
/// assertion fails, the node diff it prints names the field, which is the only
/// way a platform that cannot be run locally ever gets identified.
/// `w245_interrupted_push_test.rs` carries a copy of this test.
///
/// Nothing here is weakened by that reading: `data_blobs == 0` is what fails if
/// a changed stage is re-uploaded, `data_added == 0` is what fails if a stored
/// node's metadata moves, and the stage itself is asserted to come out of the
/// second push untouched — the product half of "no-op", which the counters
/// above can only describe from the repository's side.
#[test]
fn a_completed_push_makes_the_next_push_a_no_op() {
    let sb = Sandbox::new(12, 2_000_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Every file in the stage, with the size, mtime and sha256 it has after the
    // first push. A push is allowed to *add* to a stage — `record_writer_version`
    // writes `meta/<machine>/writer.json` before the backup — but a no-op push
    // may not rewrite what is already there, or "nothing was uploaded" would be
    // untestable from the stage's side.
    let stage_state = |sb: &Sandbox| -> BTreeMap<String, (u64, std::time::SystemTime, String)> {
        let mut state = BTreeMap::new();
        for path in stage_files(&sb.stage) {
            let meta = fs::metadata(&path).unwrap();
            let rel = path
                .strip_prefix(&sb.stage)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            state.insert(
                rel,
                (
                    meta.len(),
                    meta.modified().unwrap(),
                    sha256_hex(&fs::read(&path).unwrap()),
                ),
            );
        }
        state
    };
    let before = stage_state(&sb);
    assert!(
        !before.is_empty(),
        "the first push left no files in the stage to protect"
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
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "a second push of an unchanged stage must upload no content: {stdout}"
    );
    assert_no_tree_bytes(
        &sb,
        &stdout,
        "a second push of an unchanged stage must add no bytes at all, tree included; bytes \
         here mean a stored node's metadata moved between the two pushes",
    );
    assert_eq!(
        stage_state(&sb),
        before,
        "a second push of an unchanged stage rewrote a stage file (size, mtime or bytes \
         differ); a no-op push must leave the stage exactly as it found it: {stdout}"
    );
}

/// A field the change comparison does not read may still sit in the node the
/// push stores, and then the tree carrying it is re-serialized whenever that
/// field moves — with no content change and no counter that separates it from
/// real work. This test pins that for a field the comparison is *told* to
/// ignore: `ctime`.
///
/// `chmod` to the mode a file already has is that event on Unix: POSIX requires
/// it to bump `st_ctime`, and it leaves `st_mtime`, the mode itself and every
/// byte untouched. So by the comparison's own rules the stage is unchanged —
/// every file comes back `files_unmodified` and no content blob is written —
/// and the push must therefore add nothing at all. Before `store.rs` stopped
/// storing a ctime this test failed on this host with `data_blobs=0` and a
/// non-zero `data_added`.
///
/// It is not, it turned out, the field behind the Windows report of
/// `a_completed_push_makes_the_next_push_a_no_op`
/// (`files_unmodified=13 data_blobs=0 data_added=1690`; the same push still
/// added 1602 bytes with no ctime stored). What that platform moves is a
/// *directory*'s stored time, which
/// `a_directory_mtime_that_moves_cannot_re_serialize_a_tree` below reproduces
/// and `docs-dev/node-metadata.md` records. This test stays because it is what
/// pins the ignored-ctime half of the same policy, and because a file's times
/// are the ones change detection does read — a field the comparison skips is
/// exactly where a stored value can move unseen.
#[test]
fn a_metadata_field_the_archive_does_not_use_cannot_re_serialize_a_tree() {
    let sb = Sandbox::new(4, 200_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Same mode, same bytes, same mtime — only ctime moves.
    let mut bumped = 0usize;
    for path in stage_files(&sb.stage) {
        let permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, permissions).unwrap();
        bumped += 1;
    }
    assert!(
        bumped > 0,
        "the first push left no staged files whose ctime could be moved"
    );

    let second = sb.push();
    assert!(
        second.status.success(),
        "the second push failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let unmodified = summary_field(&stdout, "files_unmodified")
        .unwrap_or_else(|| panic!("no files_unmodified in the summary: {stdout}"));
    assert!(
        unmodified >= bumped as u64,
        "the comparison is told to ignore ctime, so every staged file must still come back \
         unmodified after one: got {unmodified} of {bumped}: {stdout}"
    );
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "moving ctime must not make the push re-read a file: {stdout}"
    );
    assert_no_tree_bytes(
        &sb,
        &stdout,
        "moving a field the archive does not use for change detection re-serialized a tree; \
         the node this push stores must not carry such a field",
    );
}

/// A directory's mtime is the time of the writes *into* it, not a property of
/// anything the archive holds — and Windows reports it lazily, so a directory
/// written to shortly before a walk can report one value to that walk and
/// another to the next one. That is what the Windows report of
/// `a_completed_push_makes_the_next_push_a_no_op` looks like once no node
/// stores a ctime (`files_unmodified=13 data_blobs=0 data_added=1602`, the same
/// 1602 in two tests whose stages differ in session count, so the tree is one
/// that does not depend on the shards).
///
/// Nothing writes into the stage between the two pushes, so the field that
/// moves there is one the *filesystem* moves: a directory node's stored time.
/// Every other stored field is either compared for files (whose mismatch shows
/// up as `files_changed`, and the Windows run counted none) or fixed by the
/// mapper. A directory's own mtime is not read by the change comparison for
/// content at all — a directory is compared through the id of the tree it
/// holds — so it is stored and never used, which is the same shape of hazard
/// `a_metadata_field_the_archive_does_not_use_cannot_re_serialize_a_tree`
/// pins for `ctime`.
///
/// Creating a file in a directory and removing it again moves that directory's
/// mtime and leaves its children exactly as they were, which is the platform's
/// event reproduced without the platform. Before the push stopped storing
/// times for directory nodes this test failed on this host with a non-zero
/// `data_added` and `data_blobs=0`.
#[test]
fn a_directory_mtime_that_moves_cannot_re_serialize_a_tree() {
    let sb = Sandbox::new(4, 200_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let dirs = stage_dirs(&sb.stage);
    assert!(
        !dirs.is_empty(),
        "the first push left no stage directories whose mtime could move"
    );
    for dir in &dirs {
        let before = fs::metadata(dir).unwrap().modified().unwrap();
        let probe = dir.join(".w242-mtime-probe");
        let mut attempts = 0;
        loop {
            fs::write(&probe, b"probe").unwrap();
            fs::remove_file(&probe).unwrap();
            if fs::metadata(dir).unwrap().modified().unwrap() != before {
                break;
            }
            // A filesystem whose timestamps are coarse, or one that reports a
            // directory's time lazily, may need a moment before the move is
            // visible — the property under test is what the *push* sees, so
            // wait for the move rather than assuming the clock moved.
            attempts += 1;
            assert!(
                attempts < 50,
                "writing into {} and removing the file again did not move its mtime; the \
                 test's premise (a directory time that can move) is gone",
                dir.display()
            );
            sleep(Duration::from_millis(20));
        }
    }

    let second = sb.push();
    assert!(
        second.status.success(),
        "the second push failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let unmodified = summary_field(&stdout, "files_unmodified")
        .unwrap_or_else(|| panic!("no files_unmodified in the summary: {stdout}"));
    assert!(
        unmodified >= sb.staged.len() as u64,
        "the probe file was removed again, so every staged file must still come back \
         unmodified: got {unmodified}: {stdout}"
    );
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "moving a directory's mtime must not make the push re-read a file: {stdout}"
    );
    assert_no_tree_bytes(
        &sb,
        &stdout,
        "a directory's mtime moved and the push re-serialized a tree; a directory is changed \
         when the tree it holds changes, not when a file is created in it and removed again",
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
    // The complete stranded packs are reused, never deleted or rewritten.
    let grew = repo_bytes(&sb.repo) - before;
    assert!(
        grew < before / 2,
        "the retry added {grew} bytes over a stranded {before}-byte repository instead of reusing its verified content"
    );
    assert_eq!(summary_field(&stdout, "data_blobs"), Some(0));

    eprintln!(
        "W245: stranded={before} bytes, retry added {grew} bytes; payload was {} bytes",
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
