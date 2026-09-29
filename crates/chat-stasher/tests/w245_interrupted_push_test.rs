//! W245 — what an interrupted push costs now that its stranded packs are made
//! reachable again.
//!
//! W242 §2 measured the defect this file now pins the fix for. rustic writes a
//! backup's packs first and its index last, so a push killed in between leaves
//! complete packs that no index file names:
//!
//! | kill point | stranded bytes | old re-run uploaded | old final repository |
//! |---|---|---|---|
//! | 1 pack finalized | 33.6 MB | 560 MB (100 %) | 454 MB |
//! | 5 packs finalized | 171.3 MB | 560 MB (100 %) | 592 MB |
//! | 10 packs finalized | 343.0 MB | 560 MB (100 %) | 764 MB |
//!
//! Every re-run reported `files_unmodified=0` and uploaded the whole payload
//! again, because a killed push leaves no parent snapshot to compare against and
//! an object id covers a fresh AEAD nonce — so re-uploading the same plaintext
//! produces different bytes under a different id and can never match the
//! stranded copy. The stranded bytes were permanent: `append_only:true` blocks
//! `repair index` and `prune`, and there is no other reclamation path.
//!
//! What changed: every open now builds its index with
//! `Repository::to_indexed_checked()`, which reads the header of any pack the
//! index does not name, so a backup's dedup test
//! (`archiver/file_archiver.rs:154`, a plain `index.has_data`) finds those blobs
//! already present and uploads none of them. See
//! `docs-dev/orphan-packs.md` for the safety argument, and `src/orphans.rs` for
//! the implementation.
//!
//! What this file asserts, at sizes the gate can afford:
//!
//! * an interrupted push still never leaves an archive that hands back bytes
//!   differing from the stage, and a retry restores full coverage;
//! * a retry after an interruption **reuses** the stranded packs: the
//!   repository grows by the new snapshot and index, not by a second copy of the
//!   payload, and no session is re-uploaded;
//! * the reuse survives the read path — a session whose content lives only in an
//!   adopted pack reads back byte-perfect (`read --all-machines`), which is the
//!   property that forces every read open to adopt too;
//! * a pack that cannot be validated is **never** adopted: the open falls back
//!   to the plain index, the push says so, and the unreadable pack is left
//!   exactly as it was.

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
            r#"{"schema_version":1,"generated":"W245 interrupted push","harnesses":[]}"#,
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

/// The `[push] stranded packs :` lines, which is where a push reports what it
/// found and what it did about it. Absent = the index named every pack in the
/// backend, which is a measurement: `push` prints nothing only when its survey
/// found nothing.
fn stranded_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| l.contains("stranded packs :"))
        .map(str::to_string)
        .collect()
}

/// Whatever `read` hands back must equal what was staged, session by session.
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

/// Repository-relative paths of every stored object, packs included: packs live
/// two levels down (`data/<xx>/<id>`), so this has to recurse.
fn repo_entries(repo: &Path) -> Vec<String> {
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
                names.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names
}

/// A pack file name is the pack's own id: 32 bytes of hex, nothing else.
///
/// The `-tmp-` suffix matters here. `LocalBackend` writes a pack to
/// `data/<xx>/<id>-tmp-` and renames it into place
/// (`rustic_backend-0.6.2` `src/local.rs:547,558`), so a file with that suffix is
/// a pack that is not finished and that no index will ever name. Treating it as
/// a stranded pack would make the race in
/// `an_interrupted_push_that_stranded_packs_is_reused_by_its_retry` fire on the
/// first byte written, when nothing has been stranded at all.
fn is_final_pack_name(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.len() == 64 && file.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every finished pack file below `repo`, sorted.
fn pack_paths(repo: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = repo_entries(repo)
        .into_iter()
        .filter(|name| name.starts_with("data/") && is_final_pack_name(name))
        .map(|name| repo.join(name))
        .collect();
    packs.sort();
    packs
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

/// Drop the index and the snapshots, keeping `config` and every pack: exactly
/// the state a kill inside the pack-window leaves. Constructing it directly is
/// what makes the cost test deterministic — the real window is a few tens of
/// milliseconds wide (`an_interrupted_push_that_stranded_packs_...` races it,
/// and is `#[ignore]`d because a race cannot be a gate).
fn strand_packs(sb: &Sandbox) -> Vec<PathBuf> {
    let stranded = pack_paths(&sb.repo);
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
    stranded
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

/// The other half of the M2-PLAN premise, and the contrast that makes the
/// stranded-pack cost readable: when the index *did* reach the disk, the next
/// push of the same stage is a genuine no-op — dedup works.
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
    // And it must claim nothing about stranded packs: the index names every pack
    // in the backend, and a survey that finds nothing prints nothing.
    assert!(
        stranded_lines(&stdout).is_empty(),
        "a repository whose index names every pack must report no stranded packs: {stdout}"
    );
}

/// The fix, at test scale: packs that reached the repository without their index
/// are **adopted**, so the retry deduplicates against them instead of uploading
/// a second copy of the payload.
///
/// W242 measured the same state at 4 sessions / 500 000 bytes and pinned the old
/// behaviour (stranded 1,045,700 B, retry added 1,047,572 B — a full second
/// copy). The assertions below are the same measurement read the other way:
/// the retry still adds the index and the snapshot, and cannot add a second copy
/// of the pack bytes.
#[test]
fn a_push_over_stranded_packs_reuses_them_instead_of_re_uploading_them() {
    let sb = Sandbox::new(4, 500_000);
    let first = sb.push();
    assert!(
        first.status.success(),
        "the first push failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);

    let second = sb.push();
    assert!(
        second.status.success(),
        "the push over a repository with no index failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);

    // The push must say what it found and what it did: this is the surface the
    // operator reads, and a silent adoption would be indistinguishable from the
    // old behaviour.
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("adopted")),
        "the push must report that it adopted the stranded packs: {stdout}"
    );
    let counted = said
        .iter()
        .find(|line| line.contains("named by no index file"))
        .unwrap_or_else(|| panic!("no stranded-pack count in: {stdout}"));
    assert!(
        counted.contains(&format!("{} pack(s)", stranded.len())),
        "the push must count every stranded pack ({}) in: {counted}",
        stranded.len()
    );

    // Nothing may be a second copy: the repository grows by the snapshot, the
    // index and the tree pack this run had to write — not by the payload.
    let grew = repo_bytes(&sb.repo) - before;
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    assert!(
        grew < payload as u64 / 4,
        "the retry added {grew} bytes over a stranded {before}-byte repository for a \
         {payload}-byte payload, which is not the reuse this fix is for"
    );

    // Every stranded pack is still there — adoption adds an index entry, it never
    // moves or rewrites a byte.
    let after = repo_entries(&sb.repo);
    for pack in &stranded {
        let rel = pack
            .strip_prefix(&sb.repo)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            after.contains(&rel),
            "the adopted pack {rel} disappeared; adoption must not move packs"
        );
    }

    // And the content came back, byte-perfect, through the ordinary read path.
    assert_eq!(
        assert_no_mismatched_content(&sb, "after the adopting push"),
        sb.staged.len()
    );
}

/// The reuse has to survive the *read* path, and this is the assertion that says
/// so out loud: after adoption the retry uploads no data pack, so the snapshot's
/// tree and its shards live only in packs no index file names — a read that
/// reads only index files then fails with "cannot ls tree" and exit 3. The read
/// path opens the way `push` does.
///
/// The `data_added == 0` assertion is what makes this test falsifiable: without
/// it the test would pass on the unfixed code too, because there the payload is
/// re-uploaded into freshly indexed packs and every blob is reachable. It fails
/// against the unfixed code (measured there: `data_added=2009841`).
#[test]
fn content_from_an_adopted_pack_reads_back_through_the_read_command() {
    let sb = Sandbox::new(2, 300_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    assert!(!stranded.is_empty(), "the first push wrote no packs");
    let second = sb.push();
    assert!(second.status.success());
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert_eq!(
        summary_field(&stdout, "data_added"),
        Some(0),
        "the retry must add no data pack at all, so the content exists only in the \
         stranded packs: {stdout}"
    );

    let read = sb.read_archive();
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert_eq!(
        read.status.code(),
        Some(0),
        "read must resolve content that only an adopted pack holds, got {:?}: {stdout}{}",
        read.status.code(),
        String::from_utf8_lossy(&read.stderr)
    );
    let archived = parse_session_shas(&stdout);
    assert_eq!(
        archived.len(),
        sb.staged.len(),
        "read must list every staged session: {stdout}"
    );
}

/// The guard, from the other side: a pack that cannot be validated as a complete
/// pack is never adopted.
///
/// This is the case a *concurrent* client produces — a pack still being written
/// to the backend is a pack whose header does not yet agree with its own size —
/// and it is why the guard on adoption is completeness rather than age. The open
/// falls back to the plain index, the push reports `NOT adopted`, and the
/// unreadable file is left exactly as it was found.
#[test]
fn an_unreadable_stranded_pack_is_reported_and_never_adopted() {
    let sb = Sandbox::new(4, 400_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);

    // Truncate the last pack in half: the state a killed remote writer leaves.
    let victim = stranded.last().unwrap().clone();
    let full = fs::read(&victim).unwrap();
    fs::write(&victim, &full[..full.len() / 2]).unwrap();
    let truncated = fs::read(&victim).unwrap();

    let second = sb.push();
    assert!(
        second.status.success(),
        "an unreadable pack must not fail the push: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("NOT adopted")),
        "the push must report the refusal rather than silently re-uploading: {stdout}"
    );
    assert!(
        !said.iter().any(|line| line.contains("adopted -")),
        "an unreadable pack must never be reported as adopted: {stdout}"
    );

    // The unreadable pack is left alone: nothing deletes or rewrites it.
    assert_eq!(
        fs::read(&victim).unwrap(),
        truncated,
        "the unreadable pack must not be modified"
    );

    // And the indexed content still reads back, byte-perfect: the refusal costs
    // the reuse, never the archive.
    assert_eq!(
        assert_no_mismatched_content(&sb, "after a refused adoption"),
        sb.staged.len()
    );
}

/// A push killed before any pack was written strands nothing, so the retry
/// uploads the whole payload — and that is the correct answer, not a defect.
///
/// W242's version of this test asserted `files_unmodified=0` as a *cost*; the
/// number is unchanged, but what it means is not: an empty pack window costs one
/// redo and no permanent bytes (`stranded_packs=0`), which is the fast half of
/// the same behaviour the adoption test pins for the slow half.
#[test]
fn a_retry_after_a_push_killed_before_any_pack_uploads_everything_again() {
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
    assert!(
        stranded_lines(&stdout).is_empty(),
        "a kill before the first pack writes strands nothing: {stdout}"
    );

    // And the payload really is all there afterwards.
    assert_eq!(
        assert_no_mismatched_content(&sb, "after the retry"),
        sb.staged.len()
    );
}

/// The real interruption, raced deliberately: kill the push the moment its first
/// pack lands at its final name — packs in the repository, index not yet written
/// — and assert the retry reuses what that push uploaded.
///
/// `#[ignore]`d because it is a race, not a gate: the window is as wide as one
/// pack write, and a machine slow enough to miss it would report "the push
/// finished or the index landed first" instead of a false pass. The constructed
/// state in `a_push_over_stranded_packs_reuses_them_instead_of_re_uploading_them`
/// is the deterministic version of the same assertion.
#[test]
#[ignore = "heavy: races the real pack-window; run explicitly"]
fn an_interrupted_push_that_stranded_packs_is_reused_by_its_retry() {
    // A payload big enough that the pack write — and therefore the window —
    // lasts long enough to be caught by a 2 ms poll.
    let sb = Sandbox::new(40, 5_000_000);
    let payload: usize = sb.staged.values().map(Vec::len).sum();

    let mut child = sb.spawn_push();
    let deadline = Instant::now() + Duration::from_secs(60);
    while pack_paths(&sb.repo).is_empty() {
        assert!(
            Instant::now() < deadline,
            "no pack appeared within 60 s; enlarge the payload"
        );
        sleep(Duration::from_millis(2));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());

    let stranded = pack_paths(&sb.repo);
    let index_written = sb.repo.join("index").exists()
        && fs::read_dir(sb.repo.join("index"))
            .map(|d| d.count())
            .unwrap_or(0)
            > 0;
    if index_written {
        eprintln!(
            "W245: the kill landed after the index, so this run measured the no-op case \
             ({} pack(s) stranded) — re-run to catch the window",
            stranded.len()
        );
        return;
    }

    let before = repo_bytes(&sb.repo);
    let retry = sb.push();
    assert!(
        retry.status.success(),
        "the retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let stdout = String::from_utf8_lossy(&retry.stdout);
    assert!(
        stranded_lines(&stdout)
            .iter()
            .any(|line| line.contains("adopted")),
        "the retry must report adopting the {}-pack stranded state: {stdout}",
        stranded.len()
    );
    let grew = repo_bytes(&sb.repo) - before;
    // The retry's own accounting of what it packed. `data_added_packed` is not
    // the number of bytes uploaded (W242 §2.4), but it is the right scale for
    // "did it write one copy or two": the stranded pack is one pack of a payload
    // whose packed size is a small multiple of a pack. Reusing it therefore
    // leaves the retry packing the remainder plus its own index and tree pack,
    // while a re-upload packs the whole payload again — measured at this size:
    // 71,733,577 B packed without adoption (`data_added=138,309,839`) against
    // 36,254,311 B with it.
    let packed = summary_field(&stdout, "data_added_packed")
        .unwrap_or_else(|| panic!("no data_added_packed in the summary: {stdout}"));
    eprintln!(
        "W245: caught the window with {} stranded pack(s) of {stranded_bytes} bytes; the retry \
         packed {packed} bytes (+{grew} on disk) for a {payload}-byte payload",
        stranded.len(),
        stranded_bytes = before,
    );
    assert!(
        packed < before * 3 / 2,
        "the retry packed {packed} bytes over a {before}-byte stranded repository: that is not \
         the remainder after reusing it, it is the whole payload again"
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after the retry"),
        sb.staged.len()
    );
}
