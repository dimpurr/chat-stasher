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
//! What changed: every open verifies the headers and blobs of unindexed packs,
//! builds index entries from those verified headers, and passes those entries
//! to rustic without allowing a second pack listing. A backup's dedup test
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

/// Which rustic backend a sandbox's repository lives on.
///
/// The adoption guard is a property of the *backend's* listing and read
/// behaviour, so the two remote-path families this project ships have to be
/// exercised, not just the local one. `LocalBackend` writes a pack to
/// `data/<xx>/<id>-tmp-` and renames it into place
/// (`rustic_backend-0.6.2` `src/local.rs:547,557`), so a listing can never show
/// a half-written pack; the OpenDAL services write to the final path
/// (`src/opendal.rs:198`, `path()`), which is the same code path the `sftp` and
/// S3 destinations take, and there a listing *can* show one.
#[derive(Clone, Copy)]
enum Repo {
    /// `--repo <path>`: `rustic_backend`'s own `LocalBackend`.
    Local,
    /// `--repo opendal:fs --option root=<path>`: OpenDAL's `fs` service, which
    /// shares the write path — and therefore the partial-file window — with the
    /// `opendal:sftp` and `opendal:s3` destinations this project actually ships.
    OpenDalFs,
}

struct Sandbox {
    dir: tempfile::TempDir,
    registry: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
    backend: Repo,
    /// session id -> the exact shard bytes written for it.
    staged: BTreeMap<String, Vec<u8>>,
}

impl Sandbox {
    /// `sessions` sessions of `bytes_each` bytes each, each shard's bytes known.
    fn new(sessions: usize, bytes_each: usize) -> Sandbox {
        Sandbox::with_backend(sessions, bytes_each, Repo::Local)
    }

    /// The same fixture on the OpenDAL `fs` service.
    fn opendal_fs(sessions: usize, bytes_each: usize) -> Sandbox {
        Sandbox::with_backend(sessions, bytes_each, Repo::OpenDalFs)
    }

    fn with_backend(sessions: usize, bytes_each: usize, backend: Repo) -> Sandbox {
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
        let repo = root.join("repo");
        if matches!(backend, Repo::OpenDalFs) {
            // The `fs` service roots itself at an existing directory: `mkfs`
            // would otherwise be the first thing the push had to do, and this
            // fixture is not testing that.
            fs::create_dir_all(&repo).unwrap();
        }
        Sandbox {
            dir,
            registry,
            repo,
            key: root.join("key.json"),
            stage,
            backend,
            staged,
        }
    }

    /// `--repo`/`--option` as this sandbox's backend needs them.
    fn repo_args(&self) -> Vec<String> {
        match self.backend {
            Repo::Local => vec!["--repo".into(), self.repo.to_string_lossy().into_owned()],
            Repo::OpenDalFs => vec![
                "--repo".into(),
                "opendal:fs".into(),
                "--option".into(),
                format!("root={}", self.repo.display()),
            ],
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
        let mut args = vec![
            "push".into(),
            "--stage".into(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".into(),
            "mbp-interrupted".into(),
        ];
        args.extend(self.repo_args());
        args.extend([
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]);
        args
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
        let mut args = vec![
            "read".to_string(),
            "--all-machines".to_string(),
            "--full-ids".to_string(),
        ];
        args.extend(self.repo_args());
        args.extend([
            "--key-file".to_string(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".to_string(),
        ]);
        self.command().args(args).output().unwrap()
    }

    /// Push with the open parked in the window an outside process cannot
    /// schedule: between the survey's pack listing and the adopting pass's.
    ///
    /// The open writes `reached` into `CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY`
    /// and waits for `go` (`orphans::hold_after_survey`), so whatever `inject`
    /// does is what the adopting pass — and nothing before it — sees. That makes
    /// the index-arrival window a fixture instead of a 100 ms race, and it is the
    /// only way to put a *pack* there.
    fn push_parked_between_survey_and_adoption(&self, inject: impl FnOnce()) -> Output {
        let rendezvous = self.dir.path().join("rendezvous");
        let child = self
            .command()
            .env("CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY", &rendezvous)
            .args(self.push_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let reached = rendezvous.join("reached");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !reached.exists() {
            assert!(
                Instant::now() < deadline,
                "the open never reached its rendezvous; a run without the hold would have \
                 finished, and this window cannot be tested by waiting"
            );
            sleep(Duration::from_millis(2));
        }
        inject();
        fs::write(rendezvous.join("go"), b"").unwrap();
        child.wait_with_output().unwrap()
    }

    /// Park immediately after verification and the last pre-adoption survey,
    /// exactly where rustic used to relist packs, then inject a valid new pack.
    fn push_parked_after_verification(&self, inject: impl FnOnce()) -> Output {
        let rendezvous = self.dir.path().join("verified-rendezvous");
        let child = self
            .command()
            .env(
                "CHAT_STASHER_TEST_HOLD_OPEN_AFTER_VERIFICATION",
                &rendezvous,
            )
            .args(self.push_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let reached = rendezvous.join("reached");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !reached.exists() {
            assert!(
                Instant::now() < deadline,
                "the open never reached the post-verification rendezvous"
            );
            sleep(Duration::from_millis(2));
        }
        inject();
        fs::write(rendezvous.join("go"), b"").unwrap();
        child.wait_with_output().unwrap()
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

/// A path relative to `root`, spelled with `/` on every platform.
///
/// The repository's own layout is what the filters here are written against —
/// `data/<xx>/<id>`, and a pack file name is the whole file name — but
/// `Path::to_string_lossy` spells the separator the host uses. On Windows that
/// is `\`, so `"data\ab\<id>".starts_with("data/")` is false and
/// `rsplit('/')` never splits: every pack the first push wrote was invisible to
/// `pack_paths`, and the three tests that strand packs died at
/// `strand_packs`'s "the first push wrote no packs to strand" on CI while
/// passing everywhere else. The repository's spelling is `/`; converting here,
/// once, is what keeps the filters from being platform-dependent.
fn unix_rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
            } else if path.strip_prefix(repo).is_ok() {
                names.push(unix_rel(&path, repo));
            }
        }
    }
    names.sort();
    names
}

/// The regression test for the separator bug above. What the filters are handed
/// is a *string*, so a Windows-spelled relative path can be fed to them from any
/// host — which is the only way this bug is visible on the machine it is written
/// on: the Windows CI run is where it bit, and the host that writes the test
/// spells every path with `/`.
///
/// Both halves are asserted, because the conversion is what is load-bearing:
/// the spelling `Path::to_string_lossy` gives on Windows names no pack to either
/// filter (`is_final_pack_name` splits on `/`, finds no separator, and measures
/// the `data\ab\` prefix as part of the file name), and the same spelling after
/// `unix_rel` names one.
#[test]
fn a_windows_spelled_relative_path_is_read_as_the_repositorys_own_layout() {
    let windows = format!(r"data\ab\{}", "0123456789abcdef".repeat(4));
    assert_eq!(
        unix_rel(Path::new(&windows), Path::new("")),
        format!("data/ab/{}", "0123456789abcdef".repeat(4)),
        "a Windows-spelled relative path must be read as the repository spells it"
    );
    assert!(
        unix_rel(Path::new(&windows), Path::new("")).starts_with("data/"),
        "the converted spelling is what `pack_paths` filters on"
    );
    assert!(
        !is_final_pack_name(&windows),
        "the host's spelling does not name a pack on its own — this is the half that \
         made every pack the first push wrote invisible to `strand_packs` on Windows"
    );
    assert!(
        is_final_pack_name(&unix_rel(Path::new(&windows), Path::new(""))),
        "the converted spelling names the pack"
    );
    assert!(
        !is_final_pack_name(&format!("{windows}-tmp-")),
        "a LocalBackend temporary pack is not a final pack, in either spelling"
    );
}

/// Where a pack's blob region ends and its header begins.
///
/// A pack is `blob, blob, … , header, header-length`: rustic writes the
/// encrypted header last and the length of that header — unencrypted, four bytes
/// little-endian — after it (`rustic_core` `src/blob/packer.rs:632-646`,
/// `repofile/packfile.rs:40-41`). So the last four bytes say how far back the header
/// starts, which is exactly what a test needs to damage every blob in a pack
/// while leaving the header, and the file's length, untouched.
fn pack_blob_region(bytes: &[u8]) -> usize {
    let end = bytes.len();
    let header_len = u32::from_le_bytes([
        bytes[end - 4],
        bytes[end - 3],
        bytes[end - 2],
        bytes[end - 1],
    ]) as usize;
    end - 4 - header_len
}

/// A pack file name is the pack's own id: 32 bytes of hex, nothing else.
///
/// The `-tmp-` suffix matters here. `LocalBackend` writes a pack to
/// `data/<xx>/<id>-tmp-` and renames it into place
/// (`rustic_backend-0.6.2` `src/local.rs:547,557`), so a file with that suffix is
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
        summary_field(&stdout, "data_blobs"),
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
        let rel = unix_rel(pack, &sb.repo);
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
/// The assertion is `data_blobs == 0`, which is what makes this test falsifiable:
/// without it the test would pass on the unfixed code too, because there the
/// payload is re-uploaded into freshly indexed packs and every blob is
/// reachable. It fails against the unfixed code (measured there:
/// `data_added=2009841`).
///
/// **Why `data_blobs` and not `data_added`.** They are different counts:
/// `rustic_core` adds `summary.data_added += self.data` for *both* packers and
/// `summary.data_blobs += self.blobs` only for the data one
/// (`rustic_core-0.12.0` `src/blob/packer.rs:389-400`, `PackerStats::apply`), so
/// `data_added` counts tree blobs as well. The Windows CI run is what showed the
/// two apart: this test failed there with `data_blobs=0 data_added=1690` — not
/// one byte of content was uploaded, and a tree (metadata) was re-serialized —
/// so the assertion was reading tree bytes as content. W252 then established
/// which stored field moves there: a *directory*'s time, which is the time of
/// the writes into the directory rather than content, is reported lazily by the
/// platform, and is stored on no counter's account — so `store.rs` no longer
/// writes one, and no longer writes a `ctime` either
/// (`docs-dev/node-metadata.md`). The counter is metadata either way, and this
/// test is about content.
#[test]
fn content_from_an_adopted_pack_reads_back_through_the_read_command() {
    let sb = Sandbox::new(2, 300_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    assert!(!stranded.is_empty(), "the first push wrote no packs");
    let before = pack_paths(&sb.repo);
    let second = sb.push();
    assert!(second.status.success());
    let stdout = String::from_utf8_lossy(&second.stdout);
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    eprintln!(
        "W245: adopted-pack retry: data_blobs={:?} data_added={:?} packed={:?} books_new={} \
         packs_before={:?} packs_after={:?}",
        summary_field(&stdout, "data_blobs"),
        summary_field(&stdout, "data_added"),
        summary_field(&stdout, "data_added_packed"),
        pack_paths(&sb.repo).len().saturating_sub(before.len()),
        before
            .iter()
            .map(|p| (unix_rel(p, &sb.repo), fs::metadata(p).unwrap().len()))
            .collect::<Vec<_>>(),
        pack_paths(&sb.repo)
            .iter()
            .map(|p| (unix_rel(p, &sb.repo), fs::metadata(p).unwrap().len()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "the retry must add no data blob at all, so the content exists only in the \
         stranded packs: {stdout}"
    );
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    assert!(
        added < payload as u64 / 4,
        "the retry added {added} bytes of data for a {payload}-byte payload: that is not the \
         tree metadata a retry may rewrite, it is the payload again: {stdout}"
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

/// The header is not the pack: a pack whose **blob bytes** were damaged, with its
/// header and its length untouched, must not be adopted either.
///
/// `to_indexed_checked()` reads the header of a pack the index does not name and
/// checks the header's own lengths against the file size. It never decrypts a
/// blob. So a pack whose ciphertext was damaged in place — what a torn write or
/// a bit-flipping store produces, and the one failure a length check cannot see —
/// passes that check, and its blobs would enter the dedup index. A later backup
/// then finds them "already present", uploads none of them, and writes a snapshot
/// that points at bytes no reader can decrypt: the reuse would cost the archive.
///
/// The bytes below are damaged all through the blob region — the bit before the
/// header — so no reader can decrypt any of them, while the header and the
/// trailing length stay exactly as they were and the file keeps its length
/// (`pack_blob_region` locates the header from that trailing length). Adoption
/// verifies a pack against the id it is stored under — that id is the SHA-256 of
/// the whole pack file (`rustic_core` `src/blob/packer.rs:762`), so any damage
/// anywhere in it moves the hash, and the header bytes themselves are covered.
#[test]
fn a_pack_with_damaged_blob_bytes_is_not_adopted_and_its_content_is_re_uploaded() {
    let sb = Sandbox::new(4, 400_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);

    // Damage every byte of every stranded pack's blob region — its ciphertext —
    // and nothing else: the header and the trailing length stay exactly as they
    // were, and the file keeps its length. This is the shape a length check
    // cannot see and a blob read cannot survive.
    let mut damaged: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for pack in &stranded {
        let mut bytes = fs::read(pack).unwrap();
        let blobs = pack_blob_region(&bytes);
        assert!(
            blobs > 0,
            "{} has a {} byte header and no blob region to damage",
            pack.display(),
            bytes.len() - blobs - 4
        );
        for byte in &mut bytes[..blobs] {
            *byte ^= 0xff;
        }
        fs::write(pack, &bytes).unwrap();
        damaged.push((pack.clone(), bytes));
    }

    let second = sb.push();
    assert!(
        second.status.success(),
        "a damaged pack must not fail the push: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("NOT adopted")),
        "the push must report the refusal rather than quietly reusing damaged bytes: {stdout}"
    );
    assert!(
        !said.iter().any(|line| line.contains("adopted -")),
        "a pack whose blobs cannot be verified must never be reported as adopted: {stdout}"
    );

    // The damaged packs are left exactly as they were found.
    for (path, bytes) in &damaged {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "{} must not be modified",
            path.display()
        );
    }

    // And their content was uploaded again. `data_added` includes plaintext
    // bytes from both data and tree packers, so compare it with a threshold well
    // below the staged payload. Repository growth is a poor measure because
    // chunk boundaries can change packed size.
    let grew = repo_bytes(&sb.repo) - before;
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    eprintln!(
        "W245: damaged blob bytes: payload={payload} data_added={added} packed={} grew={grew} \
         (repository was {before})",
        summary_field(&stdout, "data_added_packed").unwrap_or_default()
    );
    assert!(
        added as usize >= payload / 2,
        "the retry added {added} bytes of data for a {payload}-byte payload: the damaged packs \
         were treated as holding content that was already stored"
    );

    // And the archive still hands back exactly what was staged. This is the
    // assertion that reads the safety claim directly: with the verification
    // disabled, the retry adopts the damaged packs, uploads none of their
    // content, writes a snapshot pointing into them — and this read comes back
    // with zero of the four staged sessions, because nothing in the archive can
    // be decrypted any more.
    assert_eq!(
        assert_no_mismatched_content(&sb, "after a refused adoption of damaged packs"),
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

// ---------------------------------------------------------------------------
// The remote backend family. Everything above runs on `LocalBackend`, which
// writes a pack to a temporary name and renames it into place, so no listing
// can ever show a pack that is not finished. The OpenDAL services — `sftp`,
// which this project ships, and S3 — write straight to the final path, so a
// listing taken while another client is uploading *can* show one. The three
// tests below put the guard in front of that listing instead.

/// Files directly below `dir`, as `(name, bytes)`; empty when `dir` is absent.
fn dir_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, Vec<u8>)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

/// Every index file `before` captured is still there, byte for byte.
///
/// What this deliberately does **not** assert is that the index *directory*
/// gained no file. A push that re-serializes any tree writes a tree pack, and
/// that pack's index entry is written under the name of the index file's own
/// content — so a directory-level equality goes red for a tree-byte reason,
/// with nothing adopted and nothing uploaded, and says nothing about the index
/// file a push *read*. The tree-byte claim is a separate subject with its own
/// pin: `w242_interrupted_push_test::a_completed_push_makes_the_next_push_a_no_op`
/// asserts `data_added == 0` — no tree bytes either — on every platform.
fn assert_index_files_survive(dir: &Path, before: &[(String, Vec<u8>)], context: &str) {
    let after = dir_files(dir);
    for (name, bytes) in before {
        let found = after
            .iter()
            .find(|(found, _)| found == name)
            .unwrap_or_else(|| {
                panic!(
                    "{context}: the index file {name} is gone; the index directory now holds {}",
                    after
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        assert_eq!(
            &found.1, bytes,
            "{context}: the index file {name} was rewritten by a push that only reads it"
        );
    }
}

/// The index-file assertion is only worth what it can fail on: a file whose
/// bytes changed, and a file that is gone. Both are checked here against a real
/// repository, so the helper cannot go vacuous (comparing nothing, or a list to
/// itself) without these going red.
#[test]
#[should_panic(expected = "was rewritten")]
fn an_index_file_with_changed_bytes_is_reported() {
    let sb = Sandbox::opendal_fs(1, 1_000);
    assert!(sb.push().status.success());
    let index = dir_files(&sb.repo.join("index"));
    assert!(!index.is_empty(), "the first push wrote no index file");
    let mut wrong = index.clone();
    wrong[0].1.push(0x2e);
    assert_index_files_survive(&sb.repo.join("index"), &wrong, "changed bytes");
}

#[test]
#[should_panic(expected = "is gone")]
fn an_index_file_that_is_gone_is_reported() {
    let sb = Sandbox::opendal_fs(1, 1_000);
    assert!(sb.push().status.success());
    let index = dir_files(&sb.repo.join("index"));
    let mut gone = index.clone();
    gone.push(("not-a-file-the-push-wrote".to_string(), Vec::new()));
    assert_index_files_survive(&sb.repo.join("index"), &gone, "missing file");
}

/// A pack another client is still writing, seen from a backend whose listing
/// shows it: the bytes uploaded so far, at the name the pack will keep.
///
/// This is the case the guard exists for on a remote destination, and it is the
/// one `LocalBackend` cannot produce: its in-flight pack is named `<id>-tmp-`
/// and renamed, so a listing is always of finished packs. OpenDAL writes to the
/// final path, so the listing has to be assumed to contain a prefix of a pack.
/// The push must fall back to the plain index and say so rather than index a
/// pack whose bytes are half-written, and the file must be left exactly as it
/// was found — whoever is writing it is still writing it.
#[test]
fn an_incomplete_pack_in_a_remote_backends_listing_is_not_adopted() {
    let sb = Sandbox::opendal_fs(4, 400_000);
    assert!(sb.push().status.success(), "the first push failed");
    let stranded = strand_packs(&sb);

    // Half a pack: what the file looks like while it is being streamed.
    let victim = stranded.last().unwrap().clone();
    let full = fs::read(&victim).unwrap();
    fs::write(&victim, &full[..full.len() / 2]).unwrap();
    let partial = fs::read(&victim).unwrap();
    // After the truncation, not before: the store is smaller now, and the
    // number below is printed next to the re-upload it is meant to show.
    let before = repo_bytes(&sb.repo);

    let second = sb.push();
    assert!(
        second.status.success(),
        "an incomplete pack must not fail the push: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("NOT adopted")),
        "the push must report the refusal rather than index a half-written pack: {stdout}"
    );
    assert!(
        !said.iter().any(|line| line.contains("adopted -")),
        "an incomplete pack must never be reported as adopted: {stdout}"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        partial,
        "the in-flight pack must not be touched"
    );

    // The content went up again, and the archive still reads back byte-perfect.
    // `data_added` includes tree metadata, so compare it with a threshold well
    // below the staged payload. Repository growth is a poor measure because
    // chunk boundaries can change packed size.
    let grew = repo_bytes(&sb.repo) - before;
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    eprintln!(
        "W245: incomplete pack in the listing: payload={payload} data_added={added} \
         packed={} grew={grew} (repository was {before})",
        summary_field(&stdout, "data_added_packed").unwrap_or_default()
    );
    assert!(
        added as usize >= payload / 2,
        "the retry added {added} bytes of data for a {payload}-byte payload: something was \
         treated as already stored, and the pack that refused is the only candidate"
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after an incomplete pack in the listing"),
        sb.staged.len()
    );
}

/// The other half of the same race: an index file that was not there when the
/// packs were stranded, and is there when the next push opens the repository.
///
/// A second client finishing its push writes exactly this — its index file
/// lands after its packs, and it need not be a client of ours or a push of this
/// stage for our open to have to cope with it. What has to hold is that the
/// packs such an index names are not adopted a second time and not uploaded a
/// second time: they are already reachable, so the right answer is the plain
/// index (nothing reported as stranded, nothing adopted), and the push must
/// still upload no data.
///
/// The window this test cannot pin is the *arrival* of the file: whether the
/// index lands before the survey or between the survey and the final inventory
/// check is not something an outside process can schedule. The open compares
/// both inventories and refuses adoption if this index arrives during that
/// window. `an_index_file_written_while_the_open_runs` below injects it during
/// the open for the same reason.
#[test]
fn an_index_file_that_appears_after_the_packs_were_stranded_is_not_double_counted() {
    let sb = Sandbox::opendal_fs(4, 500_000);
    assert!(sb.push().status.success());
    let index = dir_files(&sb.repo.join("index"));
    assert!(!index.is_empty(), "the first push wrote no index file");
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);

    // The other client's index: put it back, and keep the packs it names.
    fs::create_dir_all(sb.repo.join("index")).unwrap();
    for (name, bytes) in &index {
        fs::write(sb.repo.join("index").join(name), bytes).unwrap();
    }

    let second = sb.push();
    assert!(
        second.status.success(),
        "a push over packs an index file names must succeed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "the index names every pack, so nothing may be uploaded again: {stdout}"
    );
    assert!(
        stranded_lines(&stdout).is_empty(),
        "nothing was stranded from this open's point of view — the index names every pack — \
         so there is nothing to report: {stdout}"
    );

    // No second copy of the payload, and every pack the index names is untouched.
    let grew = repo_bytes(&sb.repo) - before;
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    assert!(
        grew < payload as u64 / 4,
        "the retry added {grew} bytes for a {payload}-byte payload the index already names"
    );
    for pack in &stranded {
        assert!(pack.exists(), "{} disappeared", pack.display());
    }
    // The other client's index file is left as it was: the same name, the same
    // bytes. See `assert_index_files_survive` for what this must not claim.
    assert_index_files_survive(
        &sb.repo.join("index"),
        &index,
        "an index file a second client wrote must not be rewritten by a push that only reads it",
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after an index file reappeared"),
        sb.staged.len()
    );
}

/// The same index file, but *injected* into the window the timing version had
/// to race: written after the survey has listed the packs and before the
/// adopting pass lists them again.
///
/// `push_parked_between_survey_and_adoption` parks the open there, so there is
/// no sleep to lose and no "which phase saw it" to report: the survey cannot
/// have read this file (it did not exist when it ran), and the adopting pass
/// cannot have missed it.
///
/// What has to hold is the same as when the index is there from the start: the
/// packs it names are not adopted a second time, nothing is uploaded again, no
/// second copy appears on disk, and the index file is left exactly as the other
/// client wrote it.
#[test]
fn an_index_file_injected_between_the_survey_and_the_adopting_pass_is_not_double_counted() {
    let sb = Sandbox::opendal_fs(4, 500_000);
    assert!(sb.push().status.success());
    let index = dir_files(&sb.repo.join("index"));
    assert!(!index.is_empty(), "the first push wrote no index file");
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);
    let index_dir = sb.repo.join("index");

    let second = sb.push_parked_between_survey_and_adoption(|| {
        fs::create_dir_all(&index_dir).unwrap();
        for (name, bytes) in &index {
            fs::write(index_dir.join(name), bytes).unwrap();
        }
    });
    assert!(
        second.status.success(),
        "a push whose open is injected an index file must succeed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    // The survey ran before the injection, so it reported the packs as
    // stranded — which is the state this window is made of: the survey's set
    // and the adopting pass's set are not the same set of *files*, and the pass
    // is the one that reads the index.
    assert!(
        stranded_lines(&stdout)
            .iter()
            .any(|line| line.contains("named by no index file")),
        "the survey ran before the injection and must report the stranded packs: {stdout}"
    );

    // No second copy of the payload, and no content uploaded again: the index
    // file names every pack, so the dedup hit is the same one the from-the-start
    // case gets.
    let grew = repo_bytes(&sb.repo) - before;
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(0),
        "an index file that names every pack must leave the retry with nothing to upload: {stdout}"
    );
    assert!(
        grew < payload as u64 / 4,
        "the retry added {grew} bytes for a {payload}-byte payload the index already names"
    );
    for pack in &stranded {
        assert!(pack.exists(), "{} disappeared", pack.display());
    }
    assert_index_files_survive(
        &sb.repo.join("index"),
        &index,
        "the index file the other client wrote must be left as it was",
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after an index file was injected mid-open"),
        sb.staged.len()
    );
}

/// A pack that appears while the open runs is not adopted: the adopting pass is
/// only allowed to index the set this open verified.
///
/// The final inventory check closes the gap between verification and adoption.
/// This is the deterministic form of that race — a pack file written into the
/// repository while the open is parked before the final check — and the push
/// must fall back to the plain index and say so, then upload the payload again.
///
/// The injected file is a *copy* of a stranded pack under a different name,
/// which is enough for the fence: it is never read. The fence is about the full
/// inventory captured by the survey and checked before adoption.
#[test]
fn a_pack_that_appears_while_the_open_runs_is_not_adopted() {
    let sb = Sandbox::opendal_fs(4, 400_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);
    let payload: usize = sb.staged.values().map(Vec::len).sum();

    // A name the backend lists: `data/<xx>/<64 hex>`, the same shape a pack
    // this repository wrote would carry.
    let name = "0123456789abcdef".repeat(4);
    let injected = sb.repo.join("data").join(&name[..2]).join(&name);
    let source = stranded.first().unwrap().clone();
    let source_bytes = fs::read(&source).unwrap();

    let second = sb.push_parked_between_survey_and_adoption(|| {
        fs::create_dir_all(injected.parent().unwrap()).unwrap();
        fs::write(&injected, &source_bytes).unwrap();
    });
    assert!(
        second.status.success(),
        "a pack appearing mid-open must not fail the push: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("NOT adopted")),
        "the push must refuse an adoption over a pack it never verified: {stdout}"
    );
    assert!(
        !said.iter().any(|line| line.contains("adopted -")),
        "no pack may be reported as adopted when one appeared mid-open: {stdout}"
    );

    // The refusal costs the reuse, not the archive: the payload is uploaded
    // again, the injected file is left exactly as it was found, and the content
    // still reads back.
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    assert!(
        added as usize >= payload / 2,
        "the retry added {added} bytes of data for a {payload}-byte payload: the stranded packs \
         were treated as holding content that was already stored"
    );
    assert_eq!(
        fs::read(&injected).unwrap(),
        source_bytes,
        "the pack that appeared mid-open must not be modified"
    );
    let grew = repo_bytes(&sb.repo) - before;
    assert!(
        grew > 0,
        "the retry uploaded the payload again, so the store grew"
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after a pack appeared mid-open"),
        sb.staged.len()
    );
}

/// A valid pack that appears after the verification set is fixed must stay out
/// of this open's index. The pack contains different staged content, so a push
/// that adopts it would report no new data blobs.
#[test]
fn a_pack_that_appears_at_the_former_relisting_point_is_not_adopted() {
    let mut sb = Sandbox::new(4, 180_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    assert!(
        !stranded.is_empty(),
        "the first push wrote no stranded packs"
    );

    // Build a second valid pack set under the same repository key, with
    // different content from the stranded set. It will appear only after the
    // retry has verified its exact candidate set.
    let source = Sandbox::new(4, 260_000);
    fs::create_dir_all(&source.repo).unwrap();
    fs::copy(sb.repo.join("config"), source.repo.join("config")).unwrap();
    let target_keys = sb.repo.join("keys");
    let source_keys = source.repo.join("keys");
    fs::create_dir_all(&source_keys).unwrap();
    for entry in fs::read_dir(target_keys).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_file() {
            fs::copy(entry.path(), source_keys.join(entry.file_name())).unwrap();
        }
    }
    fs::copy(&sb.key, &source.key).unwrap();
    assert!(
        source.push().status.success(),
        "the source push with the shared repository key failed"
    );
    let source_packs = pack_paths(&source.repo);
    assert!(!source_packs.is_empty(), "the source push wrote no packs");

    // Replace the retry's staged payload with the source payload. Session ids
    // are stable between these fixtures; the bytes differ by size and seed.
    for (session, bytes) in &source.staged {
        let path = sb
            .stage
            .join("sessions/mbp-interrupted")
            .join(session)
            .join("000/000001.jsonl");
        fs::write(path, bytes).unwrap();
    }
    sb.staged = source.staged.clone();
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    let before = repo_bytes(&sb.repo);

    let injected = sb.push_parked_after_verification(|| {
        for pack in &source_packs {
            let relative = pack.strip_prefix(&source.repo).unwrap();
            let target = sb.repo.join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(pack, target).unwrap();
        }
    });
    assert!(
        injected.status.success(),
        "a pack appearing at the former relisting point must not fail the push: {}",
        String::from_utf8_lossy(&injected.stderr)
    );
    let stdout = String::from_utf8_lossy(&injected.stdout);
    assert_eq!(
        summary_field(&stdout, "data_blobs"),
        Some(4),
        "only the previously verified set may be adopted; the newly visible packs hold the new content and must be uploaded: {stdout}"
    );
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    assert!(
        added as usize >= payload / 2,
        "the retry added {added} bytes for a {payload}-byte payload: the new packs were treated as already stored"
    );
    for pack in source_packs {
        let target = sb.repo.join(pack.strip_prefix(&source.repo).unwrap());
        assert!(
            target.exists(),
            "the injected pack was removed: {}",
            target.display()
        );
    }
    assert_eq!(
        assert_no_mismatched_content(&sb, "after a pack appeared at the former relisting point"),
        sb.staged.len()
    );
    assert!(repo_bytes(&sb.repo) > before);
    assert!(!stranded.is_empty());
}

/// Damaged ciphertext, *and* a pack renamed to the hash of its damaged bytes:
/// the name is not the check.
///
/// `a_pack_with_damaged_blob_bytes_…` damages the whole blob region and leaves
/// the file under its old name, so comparing the file with the id it is stored
/// under catches it. That comparison is not verification: it answers "are these
/// the bytes the name claims", and a pack that was damaged and then renamed
/// answers yes. What is left is the pack's own *header*: it is AEAD ciphertext
/// under the repository key, it still parses, and it names a blob whose
/// ciphertext no longer decrypts. Only reading that blob and recomputing its id
/// from the plaintext can tell the difference, which is what
/// `crate::packcheck::verify_pack` does before anything is adopted
/// (`crates/chat-stasher/src/orphans.rs:verify_unindexed`).
///
/// One byte is flipped, in the middle of the pack's blob region — inside some
/// blob's ciphertext, since the blobs tile that region — and the pack is
/// renamed to the SHA-256 of its new bytes. The file is left exactly as it was
/// written, the refusal is reported, and the payload goes up again.
#[test]
fn a_damaged_pack_renamed_to_its_new_hash_is_not_adopted_and_its_content_is_re_uploaded() {
    let sb = Sandbox::new(4, 400_000);
    assert!(sb.push().status.success());
    let stranded = strand_packs(&sb);
    let before = repo_bytes(&sb.repo);

    // The largest stranded pack holds the content; its blob region is everything
    // before the encrypted header, which the trailing length field locates.
    let victim = stranded
        .iter()
        .max_by_key(|pack| fs::metadata(pack).unwrap().len())
        .unwrap()
        .clone();
    let mut bytes = fs::read(&victim).unwrap();
    let region = pack_blob_region(&bytes);
    assert!(
        region > 0,
        "{} has no blob region to damage",
        victim.display()
    );
    bytes[region / 2] ^= 0xff;
    let damaged = bytes.clone();

    // Rename it to the id its damaged bytes hash to, which is what a store that
    // moved the pack after damaging it — or an attacker — leaves behind. A pack
    // lives at `data/<first two hex of its id>/<id>`, so the rename is a move
    // between prefixes as well as names.
    let renamed = victim.parent().unwrap().parent().unwrap().join({
        let name = sha256_hex(&damaged);
        format!("{}/{name}", &name[..2])
    });
    assert_ne!(
        renamed, victim,
        "the damage must move the pack's own hash, or this test is not testing a rename"
    );
    fs::create_dir_all(renamed.parent().unwrap()).unwrap();
    fs::write(&renamed, &damaged).unwrap();
    fs::remove_file(&victim).unwrap();

    let second = sb.push();
    assert!(
        second.status.success(),
        "a damaged and renamed pack must not fail the push: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let said = stranded_lines(&stdout);
    assert!(
        said.iter().any(|line| line.contains("NOT adopted")),
        "the push must report the refusal rather than reuse bytes it could not read: {stdout}"
    );
    assert!(
        !said.iter().any(|line| line.contains("adopted -")),
        "a pack whose blobs cannot be read must never be reported as adopted: {stdout}"
    );

    // The renamed file is left exactly as it was found: nothing here deletes or
    // rewrites a pack, damaged or not.
    assert_eq!(
        fs::read(&renamed).unwrap(),
        damaged,
        "{} must not be modified",
        renamed.display()
    );

    // And the content went up again. `data_blobs` is the count of data blobs the
    // retry had to write, which is the claim: a dedup hit against the damaged
    // pack would have left it at zero.
    let payload: usize = sb.staged.values().map(Vec::len).sum();
    let blobs = summary_field(&stdout, "data_blobs").unwrap_or_default();
    let added = summary_field(&stdout, "data_added").unwrap_or_default();
    eprintln!(
        "W245: damaged and renamed: payload={payload} blobs={blobs} data_added={added} \
         grew={}",
        repo_bytes(&sb.repo) - before
    );
    assert!(
        added as usize >= payload / 2,
        "the retry added {added} bytes of data for a {payload}-byte payload: the damaged pack \
         was treated as holding content that was already stored"
    );
    assert_eq!(
        assert_no_mismatched_content(&sb, "after a refused adoption of a damaged, renamed pack"),
        sb.staged.len()
    );
}
