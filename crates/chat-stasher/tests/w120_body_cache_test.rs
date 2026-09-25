//! W120 · the per-machine body cache (ADR-034), exercised through the real
//! `read` path against a local repository.
//!
//! What these tests are for, in the order of the ADR's release criteria:
//!
//! 1. a second read of the same session is served from the cache, and the bytes
//!    are the same bytes (`hits` rises, `concat sha256` does not move);
//! 2. a corrupt entry is detected, deleted and re-fetched — the read either
//!    returns the correct bytes or fails, never wrong bytes;
//! 3. eviction keeps the cache inside its quota, and every read stays correct
//!    while it happens;
//! 4. a bulk read does not enter the cache, and hot entries survive a full
//!    scan;
//! 5. a session larger than a tenth of the quota is read through and not
//!    stored;
//! 6. entries are bounded by the blob, not the session, so reading a large
//!    session cannot make one file (or one allocation) as large as the session;
//! 7. two processes reading one cache at the same time both produce the right
//!    bytes.
//!
//! The fixture is synthetic: shard bodies are generated here from a
//! deterministic byte stream, no real archive, key or destination is touched,
//! and assertions read only exit codes, counts, byte lengths and sha256.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MACHINE: &str = "m-alpha";

fn cfg(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

fn session_id(n: u32) -> String {
    format!("claude-code.m-alpha.aaaaaaaa-0000-0000-0000-{n:012}")
}

/// A deterministic shard body that does **not** compress: an LCG-derived
/// alphabet soup. Compressible content would make the ciphertext orders of
/// magnitude smaller than the plaintext, and every quota assertion below would
/// then be measuring zstd rather than the cache.
fn incompressible(bytes: usize, seed: u64) -> Vec<u8> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let mut out = Vec::with_capacity(bytes + 64);
    out.extend_from_slice(br#"{"type":"user","text":""#);
    for _ in 0..bytes {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(ALPHABET[(state >> 33) as usize % ALPHABET.len()]);
    }
    out.extend_from_slice(b"\"}\n");
    out
}

/// Write `sessions` sessions of `bytes_each` into one stage.
fn write_stage(stage: &Path, count: u32, bytes_each: usize) {
    for n in 0..count {
        let body = incompressible(bytes_each, u64::from(n) + 1);
        store::write_sealed_shard_raw_with_cap(
            StageWriter::Collect,
            stage,
            MACHINE,
            &session_id(n),
            &body,
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .expect("write sealed shard");
    }
}

/// A sandbox with one local repository, one machine, and `count` sessions.
struct Sandbox {
    dir: tempfile::TempDir,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
}

impl Sandbox {
    fn new(count: u32, bytes_each: usize) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repo = root.join("repo");
        let key = root.join("key.json");
        let stage = root.join("stage");
        fs::create_dir_all(&stage).expect("stage dir");
        write_stage(&stage, count, bytes_each);
        let mk = MasterKey::new();
        store::persist_key_file(&cfg(&repo, &key), &mk).expect("persist key");
        let store = BackupStore::new(cfg(&repo, &key), MACHINE.to_string());
        assert!(
            store.push(&stage, &mk).expect("push").files_new > 0,
            "the fixture must actually archive something"
        );
        Self {
            dir,
            repo,
            key,
            stage,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn cache_dir(&self) -> PathBuf {
        self.path().join("body-cache")
    }

    /// Point the CLI at this sandbox's own cache, with `max_bytes = quota`.
    fn set_quota(&self, quota: &str) {
        self.set_cache_dir(&self.cache_dir(), quota);
    }

    /// Point the CLI at `dir` as the body-cache root, with `max_bytes = quota`.
    fn set_cache_dir(&self, dir: &Path, quota: &str) {
        let cfg_dir = self.path().join("config").join("chat-stasher");
        fs::create_dir_all(&cfg_dir).expect("config dir");
        fs::write(
            cfg_dir.join("config.toml"),
            format!(
                "[cache]\ndir = \"{}\"\nmax_bytes = \"{quota}\"\n",
                dir.display()
            ),
        )
        .expect("write config");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .env("HOME", self.path().join("home"))
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    /// `read --session <n>`, the single-session path the cache serves.
    fn read_command(&self, n: u32) -> Command {
        let mut command = self.command();
        command
            .arg("read")
            .arg("--repo")
            .arg(&self.repo)
            .arg("--key-file")
            .arg(&self.key)
            .arg("--stage")
            .arg(&self.stage)
            .arg("--machine")
            .arg(MACHINE)
            .arg("--session")
            .arg(session_id(n))
            .arg("--keep-ssh-masters");
        command
    }

    fn read(&self, n: u32) -> Output {
        self.read_command(n).output().expect("run read")
    }

    /// `read --all-machines`: the bulk read that must not touch the cache.
    fn read_all_machines(&self) -> Output {
        let mut command = self.command();
        command
            .arg("read")
            .arg("--repo")
            .arg(&self.repo)
            .arg("--key-file")
            .arg(&self.key)
            .arg("--all-machines")
            .arg("--keep-ssh-masters");
        command.output().expect("run read --all-machines")
    }

    /// `export` the one session: the other bulk operation in this family.
    fn export(&self, out: &Path) -> Output {
        let mut command = self.command();
        command
            .arg("export")
            .arg("--repo")
            .arg(&self.repo)
            .arg("--key-file")
            .arg(&self.key)
            .arg("--machine")
            .arg(MACHINE)
            .arg("--session")
            .arg(session_id(0))
            .arg("--out")
            .arg(out)
            .arg("--keep-ssh-masters");
        command.output().expect("run export")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The `sha256=` field of `read`'s `concat len` line.
fn concat_sha(output: &Output) -> String {
    let text = stdout(output);
    let line = text
        .lines()
        .find(|line| line.starts_with("[read] concat len"))
        .unwrap_or_else(|| panic!("no concat line in:\n{text}"));
    line.split("sha256=")
        .nth(1)
        .unwrap_or_else(|| panic!("no sha256 in `{line}`"))
        .trim()
        .to_string()
}

/// One `key=value` from `read`'s `[read] body cache` statistics line.
fn cache_stat(output: &Output, key: &str) -> u64 {
    let text = stdout(output);
    let line = text
        .lines()
        .find(|line| line.starts_with("[read] body cache") && line.contains("hits="))
        .unwrap_or_else(|| panic!("no body-cache statistics line in:\n{text}"));
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no `{key}=` in `{line}`"));
    value
        .parse()
        .unwrap_or_else(|e| panic!("`{key}` is not a number in `{line}`: {e}"))
}

/// The `[read] body cache` state line, before the read.
fn cache_state(output: &Output) -> String {
    let text = stdout(output);
    text.lines()
        .find(|line| line.starts_with("[read] body cache") && !line.contains("hits="))
        .unwrap_or_else(|| panic!("no body-cache state line in:\n{text}"))
        .to_string()
}

fn entry_files(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((path, size));
        }
    }
    out.sort();
    out
}

fn total_entry_bytes(root: &Path) -> u64 {
    entry_files(root).iter().map(|(_, size)| *size).sum()
}

/// A reference read with the cache switched off entirely, so every comparison
/// below is "cached bytes equal uncached bytes" rather than "two cached reads
/// agree". `max_bytes = 0` is the documented way to turn the cache off.
fn uncached_sha(sandbox: &Sandbox, n: u32) -> String {
    sandbox.set_quota("0");
    let output = sandbox.read(n);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let sha = concat_sha(&output);
    assert!(
        cache_state(&output).contains("off"),
        "with max_bytes = 0 the cache must report itself off: {}",
        cache_state(&output)
    );
    sha
}

#[test]
fn a_second_read_is_served_from_the_cache() {
    let sandbox = Sandbox::new(3, 40_000);
    sandbox.set_quota("10MB");

    let first = sandbox.read(0);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(cache_stat(&first, "hits"), 0, "a cold cache cannot hit");
    assert!(
        cache_stat(&first, "stored") > 0,
        "the first read must fill it"
    );
    assert!(cache_stat(&first, "misses") > 0);

    let entries = entry_files(&sandbox.cache_dir());
    assert_eq!(
        entries.len() as u64,
        cache_stat(&first, "stored"),
        "every stored entry must be a file on the disk"
    );

    let second = sandbox.read(0);
    assert_eq!(second.status.code(), Some(0));
    assert!(
        cache_stat(&second, "hits") > 0,
        "the second read of the same session must be served from the cache"
    );
    assert_eq!(
        concat_sha(&first),
        concat_sha(&second),
        "the cache must return exactly the bytes the remote returned"
    );
    assert_eq!(
        concat_sha(&second),
        uncached_sha(&sandbox, 0),
        "cached and uncached reads must agree byte for byte"
    );
}

#[test]
fn a_corrupt_entry_is_refetched_not_served() {
    let sandbox = Sandbox::new(1, 40_000);
    // The reference read turns the cache off, so the quota has to be set again
    // before the reads that are supposed to fill it.
    let reference = uncached_sha(&sandbox, 0);
    sandbox.set_quota("10MB");

    let first = sandbox.read(0);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(concat_sha(&first), reference);

    // Flip one bit in the last byte of every entry, so whichever blob the
    // session's bytes live in is corrupt.
    let entries = entry_files(&sandbox.cache_dir());
    assert!(
        !entries.is_empty(),
        "the first read must have filled the cache"
    );
    for (path, _) in &entries {
        let mut raw = fs::read(path).expect("read entry");
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        fs::write(path, raw).expect("corrupt entry");
    }

    let second = sandbox.read(0);
    assert_eq!(second.status.code(), Some(0));
    assert!(
        cache_stat(&second, "corrupt") > 0,
        "the corruption must be detected rather than served"
    );
    assert_eq!(
        concat_sha(&second),
        reference,
        "the read must fall back to the remote and return the correct bytes"
    );

    // The corrupt entries were replaced, so the cache works again.
    let third = sandbox.read(0);
    assert_eq!(third.status.code(), Some(0));
    assert!(cache_stat(&third, "hits") > 0);
    assert_eq!(concat_sha(&third), reference);
}

#[test]
fn eviction_keeps_the_cache_inside_its_quota() {
    // Quota 400 kB, 20 sessions of 30 kB: a tenth of the quota is 40 kB, so
    // every session is allowed in, and together they are 50% over.
    let sandbox = Sandbox::new(20, 30_000);
    // The reference reads happen inside the loop, one per session: each one
    // turns the cache off, so the quota is restored after each.
    sandbox.set_quota("400kB");

    for n in 0..20 {
        let output = sandbox.read(n);
        assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
        assert_eq!(
            concat_sha(&output),
            uncached_sha(&sandbox, n),
            "session {n} must read back correctly while the cache is evicting"
        );
        sandbox.set_quota("400kB");
        assert!(
            total_entry_bytes(&sandbox.cache_dir()) <= 400_000,
            "the cache must never exceed its quota (session {n})"
        );
    }

    // The sessions read most recently are the ones still held: the last read
    // must hit, because eviction is least-recently-used.
    let last = sandbox.read(19);
    assert_eq!(last.status.code(), Some(0));
    assert!(
        cache_stat(&last, "hits") > 0,
        "the most recently read session must still be cached"
    );
}

#[test]
fn a_bulk_read_does_not_enter_the_cache() {
    let sandbox = Sandbox::new(3, 40_000);
    sandbox.set_quota("10MB");

    // Fill the cache with one session's body: this is the "hot" entry.
    let filled = sandbox.read(0);
    assert_eq!(filled.status.code(), Some(0));
    let bytes_before = total_entry_bytes(&sandbox.cache_dir());
    let entries_before = entry_files(&sandbox.cache_dir());
    assert!(bytes_before > 0);

    // `read --all-machines` reads every session of every machine — a full scan.
    let bulk = sandbox.read_all_machines();
    assert_eq!(bulk.status.code(), Some(0), "{}", stdout(&bulk));
    assert!(
        cache_state(&bulk).contains("bulk read"),
        "a bulk read must say why it is not using the cache: {}",
        cache_state(&bulk)
    );
    assert_eq!(
        total_entry_bytes(&sandbox.cache_dir()),
        bytes_before,
        "a full scan must not add anything to the cache"
    );
    assert_eq!(
        entry_files(&sandbox.cache_dir()),
        entries_before,
        "and must not remove or rewrite what was there"
    );

    // `export` is bulk for the same reason.
    let out = sandbox.path().join("export-out");
    let exported = sandbox.export(&out);
    assert_eq!(exported.status.code(), Some(0), "{}", stdout(&exported));
    assert_eq!(
        total_entry_bytes(&sandbox.cache_dir()),
        bytes_before,
        "export must not add anything to the cache either"
    );

    // ... and the hot entry is still hot: the scan did not evict it.
    let again = sandbox.read(0);
    assert_eq!(again.status.code(), Some(0));
    assert!(
        cache_stat(&again, "hits") > 0,
        "the session read before the full scan must still be cached after it"
    );
}

#[test]
fn a_session_over_a_tenth_of_the_quota_is_not_stored() {
    // 30 kB session against a 200 kB quota: a tenth is 20 kB, so it is refused.
    let sandbox = Sandbox::new(1, 30_000);
    sandbox.set_quota("200kB");
    let reference = uncached_sha(&sandbox, 0);
    sandbox.set_quota("200kB");

    let first = sandbox.read(0);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(concat_sha(&first), reference);
    assert_eq!(
        cache_stat(&first, "stored"),
        0,
        "a session larger than a tenth of the quota must not be stored"
    );
    assert!(cache_stat(&first, "skipped_session") > 0);
    assert!(
        entry_files(&sandbox.cache_dir()).is_empty(),
        "and it must leave no entries behind"
    );

    // The same session under a quota it fits well inside is stored: the rule is
    // about the size of the session, not about the session.
    sandbox.set_quota("10MB");
    let second = sandbox.read(0);
    assert_eq!(second.status.code(), Some(0));
    assert!(cache_stat(&second, "stored") > 0);
    let third = sandbox.read(0);
    assert!(cache_stat(&third, "hits") > 0);
    assert_eq!(concat_sha(&third), reference);
}

#[test]
fn entries_are_bounded_by_the_blob_not_by_the_session() {
    // One 6 MB session with a quota that holds it whole. If the cache stored
    // bodies, this would be one 6 MB file; it must be several bounded ones,
    // because that is what keeps a large read's memory bounded by the blob.
    let sandbox = Sandbox::new(1, 6_000_000);
    sandbox.set_quota("100MB");
    let output = sandbox.read(0);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(cache_stat(&output, "stored") > 0);

    let entries = entry_files(&sandbox.cache_dir());
    assert!(
        entries.len() > 1,
        "a 6 MB session must be stored as several blob-sized entries, not one: \
         entries={} stored={} misses={} hits={} corrupt={} too_large={} session_skips={} errors={} usage={}",
        entries.len(),
        cache_stat(&output, "stored"),
        cache_stat(&output, "misses"),
        cache_stat(&output, "hits"),
        cache_stat(&output, "corrupt"),
        cache_stat(&output, "skipped_too_large"),
        cache_stat(&output, "skipped_session"),
        cache_stat(&output, "errors"),
        cache_stat(&output, "usage"),
    );
    let largest = entries.iter().map(|(_, size)| *size).max().unwrap_or(0);
    assert!(
        largest < 6_000_000,
        "no single entry may be as large as the session (largest was {largest})"
    );
}

#[test]
fn several_processes_read_through_one_cache_at_once() {
    // 30 kB sessions with a 400 kB quota: a tenth of the quota is 40 kB, so
    // every session may be stored, and fourteen of them are 20 kB more than the
    // quota holds — which is what keeps an eviction pass running underneath the
    // concurrent writers below.
    let sandbox = Sandbox::new(14, 30_000);
    let reference: Vec<String> = (0..4).map(|n| uncached_sha(&sandbox, n)).collect();
    sandbox.set_quota("400kB");

    // Bring the cache to its quota first, so the concurrent round starts with
    // the eviction path already live rather than with an empty directory.
    for n in 0..14u32 {
        assert_eq!(sandbox.read(n).status.code(), Some(0));
    }
    assert!(total_entry_bytes(&sandbox.cache_dir()) <= 400_000);

    // Four reads at once, all sharing one cache directory.
    let mut children = Vec::new();
    for n in 0..4u32 {
        let mut command = sandbox.read_command(n);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        children.push((n, command.spawn().expect("spawn read")));
    }
    for (n, child) in children {
        let output = child.wait_with_output().expect("wait for read");
        assert!(
            output.status.success(),
            "concurrent read of session {n} failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // Every session still reads back correctly, whichever process stored which
    // entry, and the shared cache is still inside its quota.
    for n in 0..4u32 {
        let output = sandbox.read(n);
        assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
        assert_eq!(
            concat_sha(&output),
            reference[n as usize],
            "session {n} must be correct after concurrent reads through one cache"
        );
        assert!(total_entry_bytes(&sandbox.cache_dir()) <= 400_000);
    }

    // The entries the concurrent writers left behind are usable, which is what
    // the digest check on every hit proves from the inside.
    let hits: u64 = (0..4u32)
        .map(|n| cache_stat(&sandbox.read(n), "hits"))
        .sum();
    assert!(hits > 0, "the concurrent fills must leave usable entries");
}

/// `cache` reports where the cache is, what it may use and what it holds — and
/// `cache clear` empties it without touching anything else.
///
/// The three "no cache here" states are worded apart on purpose: a machine with
/// no cache directory yet, a cache switched off in the config, and a cache that
/// could not be measured are different findings, and only the first is the
/// normal state of a machine that has not read a session yet.
#[test]
fn the_cache_command_reports_and_clears() {
    let sandbox = Sandbox::new(1, 40_000);

    // The uncached reference read first: it also leaves the cache directory
    // untouched, which is what the "unknown" assertion below depends on.
    let reference = uncached_sha(&sandbox, 0);

    // Nothing has been cached yet: the occupancy is unknown, not zero.
    sandbox.set_quota("10MB");
    let fresh = sandbox.command().arg("cache").output().expect("run cache");
    assert_eq!(fresh.status.code(), Some(0));
    let text = stdout(&fresh);
    assert!(
        text.contains("unknown (no cache directory yet)"),
        "a cache that was never written must read as unknown, not as 0 B:\n{text}"
    );

    // Fill it, then ask again.
    assert_eq!(sandbox.read(0).status.code(), Some(0));
    let filled = sandbox.command().arg("cache").output().expect("run cache");
    let text = stdout(&filled);
    assert!(
        text.contains(&format!("quota          : {}", 10_000_000)),
        "the report must name the quota:\n{text}"
    );
    assert!(
        !text.contains("unknown (no cache directory yet)"),
        "an existing cache must be measured:\n{text}"
    );
    let bytes_before = total_entry_bytes(&sandbox.cache_dir());
    assert!(bytes_before > 0);

    // Clear it: the entries go, and the archive is not consulted at all.
    let cleared = sandbox
        .command()
        .args(["cache", "clear"])
        .output()
        .expect("run cache clear");
    assert_eq!(cleared.status.code(), Some(0), "{}", stdout(&cleared));
    assert!(
        stdout(&cleared).contains("cleared 1 entries"),
        "the clear must report what it removed:\n{}",
        stdout(&cleared)
    );
    assert_eq!(total_entry_bytes(&sandbox.cache_dir()), 0);

    // And the session still reads back correctly, from the destination.
    sandbox.set_quota("10MB");
    let after = sandbox.read(0);
    assert_eq!(after.status.code(), Some(0), "{}", stdout(&after));
    assert_eq!(
        concat_sha(&after),
        reference,
        "a cleared cache must fall back to the destination and return the same bytes"
    );
}

/// A `[cache] dir` that points at a directory this cache did not create is
/// refused, and nothing in that directory is touched.
///
/// The cache's entries are disposable by construction; the contents of a
/// directory the user pointed at by mistake are not. So the fixed direction is
/// to refuse loudly — on the report, on the `read` that would have filled it,
/// and above all on `cache clear`, the one command that deletes.
#[test]
fn the_cache_refuses_a_directory_it_did_not_create() {
    let sandbox = Sandbox::new(1, 40_000);
    // A directory that exists, is not the cache's, and holds a file that was
    // never a cache entry: no marker, so nothing here is chat-stasher's.
    let dir = sandbox.path().join("someone-elses-dir");
    fs::create_dir_all(&dir).expect("mkdir");
    let keep = dir.join("keep-me.txt");
    fs::write(&keep, b"not a cache entry").expect("write");

    sandbox.set_cache_dir(&dir, "10MB");

    // The report says what it found, without measuring somebody else's bytes as
    // a cache occupancy.
    let report = sandbox.command().arg("cache").output().expect("run cache");
    let text = stdout(&report);
    assert!(
        !text.contains("occupancy      : ") || text.contains("unknown (not a chat-stasher"),
        "the report must not present another directory's bytes as cache occupancy:\n{text}"
    );
    assert!(
        text.contains("not a chat-stasher body cache"),
        "the report must say the directory is not this cache's:\n{text}"
    );

    // Clearing refuses, and refuses before deleting anything.
    let cleared = sandbox
        .command()
        .args(["cache", "clear"])
        .output()
        .expect("run cache clear");
    assert_eq!(
        cleared.status.code(),
        Some(2),
        "refusing to clear a directory that is not the cache's must not look like \
         a successful clear:\n{}",
        stdout(&cleared)
    );
    let refusal = format!("{}{}", stdout(&cleared), stderr(&cleared));
    assert!(
        refusal.contains(".chat-stasher-body-cache"),
        "the refusal must name the marker it looked for:\n{refusal}"
    );
    assert!(
        keep.exists(),
        "`cache clear` deleted a file that is not a cache entry"
    );

    // A read still works — the cache is a speed-up, never a dependency — and it
    // reports why it went to the destination instead, and writes nothing here.
    let read = sandbox.read(0);
    assert_eq!(read.status.code(), Some(0), "{}", stdout(&read));
    assert!(
        cache_state(&read).contains("off"),
        "the read must report the cache as off, not as on: {}",
        cache_state(&read)
    );
    let state = cache_state(&read);
    assert!(
        state.contains("not a chat-stasher body cache"),
        "the read must say why the cache is off: {state}"
    );
    assert_eq!(
        entry_files(&dir)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<PathBuf>>(),
        vec![keep.clone()],
        "the read stored entries in a directory that is not the cache's"
    );
    assert!(keep.exists(), "the read touched a file it did not write");
}

/// A `[cache]` value the parser cannot read turns the cache off and says why.
///
/// It must not activate the cache at the documented default quota — the user
/// wrote a quota, and reporting a different one as if they had written it is the
/// same class of mistake as recording an unknown as a number — and it must not
/// stop the read from working.
#[test]
fn an_unreadable_cache_quota_turns_the_cache_off() {
    let sandbox = Sandbox::new(1, 40_000);
    // `50G` is refused by the size parser: `GB` is 10⁹ and `GiB` is 2³⁰, and a
    // bare `G` is neither.
    let cfg_dir = sandbox.path().join("config").join("chat-stasher");
    fs::create_dir_all(&cfg_dir).expect("config dir");
    fs::write(
        cfg_dir.join("config.toml"),
        "[cache]\nmax_bytes = \"50G\"\n",
    )
    .expect("write config");

    let read = sandbox.read(0);
    assert_eq!(
        read.status.code(),
        Some(0),
        "a cache that cannot be configured must not fail a read:\n{}",
        stdout(&read)
    );
    let state = cache_state(&read);
    assert!(
        state.contains("off"),
        "an unreadable quota disables the cache rather than choosing one: {state}"
    );
    assert!(
        state.contains("50G"),
        "the state line must quote the value to fix: {state}"
    );

    // The cache the *absent* section would have meant is the default one, and
    // nothing may have been written to it.
    let default_root = sandbox
        .path()
        .join("cache")
        .join("chat-stasher")
        .join("body");
    assert!(
        !default_root.exists(),
        "a mistyped quota must not activate the cache at the default location: {}",
        default_root.display()
    );

    // The stderr warning names the file, so the fix is in the message.
    let warning = stderr(&read);
    assert!(
        warning.contains("[cache]"),
        "the warning must name the section to fix:\n{warning}"
    );
}

/// The second read of one session must fetch **no body bytes at all**: not
/// "fewer", not "faster", but none — every body lookup hits.
///
/// This is the property criterion ① is really about. On a remote destination
/// the 22.6 s a warm 107 MB read costs is the download (W117 measured ~4.7 MB/s
/// against the Storage Box with zero cache involvement); if a second read
/// reports zero body misses, those bytes did not cross the wire, and what is
/// left is the fixed per-run cost of opening the repository — which this cache
/// neither causes nor removes.
#[test]
fn a_second_read_fetches_no_body_bytes() {
    let sandbox = Sandbox::new(1, 40_000);
    sandbox.set_quota("10MB");

    let first = sandbox.read(0);
    assert_eq!(first.status.code(), Some(0));
    assert!(cache_stat(&first, "misses") > 0, "a cold cache must fetch");

    let second = sandbox.read(0);
    assert_eq!(second.status.code(), Some(0));
    assert!(
        cache_stat(&second, "hits") > 0,
        "the second read must be served from the cache"
    );
    assert_eq!(
        cache_stat(&second, "misses"),
        0,
        "the second read of an unchanged session must not fetch a single body block"
    );
}

/// The cache holds the destination's ciphertext, and nothing else.
///
/// ADR-034 forbids plaintext on the disk, and that is checkable: the fixture's
/// shard body contains a long marker that would appear verbatim in any file
/// holding the conversation, so the marker must be absent from every byte the
/// cache wrote. (It is synthetic text, so searching for it reads nothing
/// private.)
#[test]
fn the_cache_writes_ciphertext_and_no_plaintext() {
    let sandbox = Sandbox::new(1, 40_000);
    sandbox.set_quota("10MB");
    let output = sandbox.read(0);
    assert_eq!(output.status.code(), Some(0));
    assert!(cache_stat(&output, "stored") > 0);

    // The fixture's alphabet soup is the plaintext; take a long slice of what
    // the stage holds for that session and search every cache entry for it.
    // The stage keeps shards in bucketed subdirectories, so walk the session
    // directory and take its largest `.jsonl` file.
    let session_dir = sandbox
        .stage
        .join("sessions")
        .join(MACHINE)
        .join(session_id(0));
    let stage_file = entry_files(&session_dir)
        .into_iter()
        .filter(|(path, _)| path.extension().map(|x| x == "jsonl").unwrap_or(false))
        .max_by_key(|(_, size)| *size)
        .map(|(path, _)| path)
        .expect("a sealed shard on disk");
    let plaintext = fs::read(&stage_file).expect("read shard");
    let marker = &plaintext[2..plaintext.len() - 2];

    let mut checked = 0u64;
    for (path, _) in entry_files(&sandbox.cache_dir()) {
        let raw = fs::read(&path).expect("read entry");
        assert!(
            !raw.windows(marker.len()).any(|w| w == marker),
            "an entry contains the conversation's plaintext: {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "the cache wrote no entries to check");
}
