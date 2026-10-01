//! W243 · ADR-034 release criterion ①, measured for real against a local
//! archive: wall-clock time to `read` one ~100 MB session, cold (cache empty,
//! body fetched from the repository) against warm (cache full, zero body
//! misses), several runs each, median reported.
//!
//! This is a manual measurement, not a CI assertion — the ~100 MB fixture is
//! far too large to build in every test run. Run it explicitly with:
//!
//! ```sh
//! cargo test --release --test w243_wallclock_test -- --ignored --nocapture
//! ```
//!
//! (release, so the crypto and the reading loop are the ones a user gets).
//! The only assertions are structural and cheap: every warm read must report
//! zero body misses and a matching digest — the wall-clock numbers are printed,
//! not asserted, because timings vary by machine.
//!
//! The archive is synthetic: shard bodies are generated from a deterministic,
//! **incompressible** byte stream via the project's own sealed-shard + `push`
//! path, so the repository holds ~100 MB of ciphertext roughly equal to the
//! stated plaintext. No real conversation, key or destination is touched.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "m-alpha";
/// Plaintext bytes of the measured session, spread over this many shards.
const SHARD_BYTES: usize = 2_048_000;
const SHARD_COUNT: u32 = 50; // ~97.7 MiB ≈ 100 MB of plaintext
const SAMPLES: u32 = 5;

/// Metadata cache rooted in the sandbox (W289), so the ~100 MB fixture push
/// and the timed reads below never touch the real user cache; a wall-clock
/// measurement that also grows the machine's cache directory would be timing
/// something nobody asked it to time.
fn cfg(repo: &Path, key: &Path, cache: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(cache.join("rustic-cache")),
        no_cache: false,
    }
}

fn session_id(n: u32) -> String {
    format!("claude-code.m-alpha.aaaaaaaa-0000-0000-0000-{n:012}")
}

/// A deterministic shard body that does **not** compress: an LCG-derived
/// alphabet soup, so ciphertext is ~ the plaintext size and the quota/cache
/// assertions measure the cache rather than zstd.
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

struct Sandbox {
    dir: tempfile::TempDir,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let stage = root.join("stage");
        fs::create_dir_all(&stage).expect("stage dir");
        // One session spread over many shards: this defeats the destination's
        // drawn chunking polynomial in the same way the w120 tests do, and it
        // is the shape a session that accrues many collects actually has.
        for n in 0..SHARD_COUNT {
            let seed = u64::from(n) + 1;
            let body = incompressible(SHARD_BYTES, seed);
            store::write_sealed_shard_raw_with_cap(
                StageWriter::Collect,
                &stage,
                MACHINE,
                &session_id(0),
                &body,
                store::DEFAULT_SHARD_BUCKET_CAP,
            )
            .expect("write sealed shard");
        }
        let repo = root.join("repo");
        let key = root.join("key.json");
        let mk = rustic_core::repofile::MasterKey::new();
        store::persist_key_file(&cfg(&repo, &key, root), &mk).expect("persist key");
        let store = BackupStore::new(cfg(&repo, &key, root), MACHINE.to_string());
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

    fn set_quota(&self, quota: &str) {
        let cfg_dir = self.path().join("config").join("chat-stasher");
        fs::create_dir_all(&cfg_dir).expect("config dir");
        fs::write(
            cfg_dir.join("config.toml"),
            format!(
                "[cache]\ndir = \"{}\"\nmax_bytes = \"{quota}\"\n",
                self.cache_dir().display()
            ),
        )
        .expect("write config");
    }

    /// Hard-clear the cache directory (its marker included), so the next read
    /// is a true cold first-read that must fetch every body block.
    fn clear_cache(&self) {
        let dir = &self.cache_dir();
        if dir.exists() {
            fs::remove_dir_all(dir).expect("remove cache dir");
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .env("HOME", self.path().join("home"))
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&self.path().join("home")),
            )
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    /// `read --session 0` scoped to this sandbox; returns (elapsed_ms, output).
    fn read_once(&self) -> (u128, Output) {
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
            .arg(session_id(0))
            .arg("--keep-ssh-masters");
        let start = Instant::now();
        let out = command.output().expect("run read");
        let ms = start.elapsed().as_millis();
        (ms, out)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

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

fn median(mut ms: Vec<u128>) -> u128 {
    ms.sort_unstable();
    ms[ms.len() / 2]
}

#[test]
#[ignore = "manual wall-clock measurement; ~100 MB fixture is too large for CI"]
fn measure_100mb_session_cold_vs_warm_local() {
    let sandbox = Sandbox::new();
    // The cache must hold the whole ~100 MB session and a session no larger
    // than a tenth of the quota may be stored: 2 GiB quota → 10% = 215 MiB,
    // comfortably above the ~98 MiB session.
    sandbox.set_quota("2GiB");

    println!(
        "archive: ~{:.0} MiB plaintext over {SHARD_COUNT} shards of {SHARD_BYTES} B each",
        (SHARD_BYTES as f64 * SHARD_COUNT as f64) / 1_048_576.0
    );
    println!("cache: quota 2GiB (10% = 215 MiB > session, so the ~98 MiB session may be stored)");
    println!(
        "{:<6} {:<6} {:>8} {:>6} {:>6}",
        "run", "kind", "ms", "hits", "misses"
    );

    let mut cold_ms = Vec::new();
    for i in 0..SAMPLES {
        sandbox.clear_cache();
        let (ms, out) = sandbox.read_once();
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
        println!(
            "{:<6} {:<6} {:>8} {:>6} {:>6}",
            i + 1,
            "cold",
            ms,
            cache_stat(&out, "hits"),
            cache_stat(&out, "misses"),
        );
        cold_ms.push(ms);
    }

    // The cache is already hot from the last cold read; this sample just
    // confirms the warm path works and pins the reference digest.
    let (_, fill) = sandbox.read_once();
    assert_eq!(fill.status.code(), Some(0), "{}", stdout(&fill));
    let reference = concat_sha(&fill);

    let mut warm_ms = Vec::new();
    for i in 0..SAMPLES {
        let (ms, out) = sandbox.read_once();
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
        assert_eq!(
            concat_sha(&out),
            reference,
            "a warm read must return exactly the bytes the cold read returned"
        );
        assert!(
            cache_stat(&out, "hits") > 0,
            "a warm read must be served from the cache"
        );
        assert_eq!(
            cache_stat(&out, "misses"),
            0,
            "a second read of an unchanged session must not fetch a single body block"
        );
        println!(
            "{:<6} {:<6} {:>8} {:>6} {:>6}",
            i + 1,
            "warm",
            ms,
            cache_stat(&out, "hits"),
            cache_stat(&out, "misses"),
        );
        warm_ms.push(ms);
    }

    let c = median(cold_ms.clone());
    let w = median(warm_ms.clone());
    println!(
        "median cold = {c} ms · median warm = {w} ms · warm {:.1}% of cold",
        (w as f64 * 100.0) / c as f64
    );
    println!("cold runs: {:?}\nwarm runs: {:?}", cold_ms, warm_ms);
}
