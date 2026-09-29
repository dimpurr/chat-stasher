//! W242 item 3 — RED. Two ways the reader fails the exit-code contract when a
//! stored **metadata** pack is truncated, which is exactly the half-written
//! object ADR-016 Decision 4 narrowed its risk to.
//!
//! The contract these tests assert is the project's own (CLAUDE.md invariant 2,
//! and `verify`'s documented exit codes): an archive that could not be read
//! fully must exit **3** — "did not finish reading" — so that "we could not
//! check" stays distinguishable from "there is nothing there". A crash and a
//! hang are neither: the first looks like a broken tool, the second looks like
//! a stuck one, and neither tells the user that the archive is unreadable.
//!
//! Both tests are `#[ignore]`d because both fail today. That is deliberate —
//! they are the RED half of "write a test that pins the safe behaviour, or a
//! RED test plus a fix if it is unsafe", and the fix is not ours to make: the
//! panic and the stall are inside the pinned `rustic_core` /`rustic_backend`
//! reader, reached through its metadata cache. They are kept as executable
//! documentation of the defect and can be run with
//! `cargo test -p chat-stasher --test w242_archive_corruption_red_test -- --ignored`.
//!
//! Measured 2026-09-29, on a repository holding one 20 KB session, with the
//! default configuration (metadata cache enabled) and a **cold** cache:
//!
//! * `read --all-machines` → exit **101**, a Rust panic:
//!   `bytes-1.12.1/src/bytes.rs:374: range end out of bounds: 3139 <= 1833`.
//! * `verify --level all` → did not finish within 300 s. On a repository whose
//!   whole payload is 20 KB.
//!
//! Both are specific to the cache path. With `rustic_no_cache = true` the same
//! corruption is reported correctly: `read` exits 3. The green sibling file
//! (`w242_archive_integrity_test.rs`) runs with the cache off for that reason,
//! and its assertions are about the archive's contents, not about this defect.

use chat_stasher::store::{BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a reader may take before the test calls it a stall. Generous: the
/// green path reads this repository in well under a second.
const READER_LIMIT: Duration = Duration::from_secs(60);

const EXIT_DID_NOT_FINISH: i32 = 3;

struct Fixture {
    dir: tempfile::TempDir,
    stage: PathBuf,
    cfg: StoreConfig,
    machine: String,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let machine = "mbp-truncated-meta".to_string();
        let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
        let stage = root.join("stage");
        let shards = stage
            .join("sessions")
            .join(&machine)
            .join(&session)
            .join("000");
        fs::create_dir_all(&shards).unwrap();
        // Compressible content, so the *metadata* pack is the large one: that is
        // the pack layout this test is about.
        let content: String = (0..20_000)
            .map(|i| (b'!' + (i % 60) as u8) as char)
            .collect();
        fs::write(
            shards.join("000001.jsonl"),
            format!(
                r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{content}"}},"uuid":"u1","timestamp":"2025-07-01T10:00:00Z"}}"#
            ) + "\n",
        )
        .unwrap();

        let cfg = StoreConfig {
            repo_root: root.join("repo").to_string_lossy().into_owned(),
            key_file: root.join("masterkey.json"),
            connections: 1,
            options: Default::default(),
            cache_dir: Some(root.join("cache")),
            no_cache: false,
        };
        let mk = MasterKey::new();
        chat_stasher::store::persist_key_file(&cfg, &mk).unwrap();
        BackupStore::new(cfg.clone(), machine.clone())
            .push(&stage, &mk)
            .unwrap();
        Fixture {
            dir,
            stage,
            cfg,
            machine,
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// Truncate every pack to half its length, leaving `config`, the index and
    /// the snapshot intact — a repository whose objects are all present under
    /// their final names and none of them complete.
    fn truncate_every_pack(&self) {
        let mut stack = vec![self.repo().join("data")];
        let mut truncated = 0;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = fs::read(&path).unwrap();
                    fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
                    truncated += 1;
                }
            }
        }
        assert!(truncated > 0, "the fixture wrote no packs to truncate");
    }

    /// The real binary, with every ambient path redirected into the fixture and
    /// **no** cache configuration: the default a user gets.
    fn cli(&self) -> Command {
        let root = self.dir.path();
        for sub in ["home", "config", "data-xdg", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W242 red","harnesses":[]}"#,
        )
        .unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data-xdg"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("CHAT_STASHER_REGISTRY", &registry);
        cmd
    }

    fn repo_args(&self) -> Vec<String> {
        vec![
            "--repo".into(),
            self.repo().to_string_lossy().into_owned(),
            "--key-file".into(),
            self.cfg.key_file.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]
    }

    /// Run to completion, or kill and report `None` if it outlives the limit.
    /// A test that could hang the gate must not be able to.
    fn run_bounded(&self, args: &[&str]) -> Option<i32> {
        let mut child: Child = self
            .cli()
            .args(args)
            .args(self.repo_args())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + READER_LIMIT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code();
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                return None;
            }
            sleep(Duration::from_millis(25));
        }
    }
}

/// `read` must report "did not finish reading", not crash.
#[test]
#[ignore = "RED 2026-09-29: with the default metadata cache and a cold cache, a truncated \
            pack makes `read` panic (exit 101) instead of exiting 3 — the measurements are \
            in this file's module doc, and `w242_archive_integrity_test.rs` is the green \
            control with the cache off"]
fn read_over_a_truncated_pack_must_exit_3_not_panic() {
    let fx = Fixture::new();
    fx.truncate_every_pack();
    assert_eq!(
        fx.run_bounded(&["read", "--all-machines"]),
        Some(EXIT_DID_NOT_FINISH),
        "an unreadable archive must exit {EXIT_DID_NOT_FINISH} (did not finish reading); \
         a panic exit (101) or a stall (None) is not a diagnosis"
    );
}

/// `verify` exists to find exactly this. It must report it, not stall.
#[test]
#[ignore = "RED 2026-09-29: with the default metadata cache and a cold cache, `verify` over a \
            truncated pack does not finish (measured: >300 s on a 20 KB payload) — the \
            measurements are in this file's module doc"]
fn verify_over_a_truncated_pack_must_exit_3_not_stall() {
    let fx = Fixture::new();
    fx.truncate_every_pack();
    let stage = fx.stage.to_string_lossy().into_owned();
    let machine = fx.machine.clone();
    assert_eq!(
        fx.run_bounded(&[
            "verify",
            "--level",
            "all",
            "--stage",
            &stage,
            "--machine",
            &machine,
        ]),
        Some(EXIT_DID_NOT_FINISH),
        "verify must report an unreadable archive with exit {EXIT_DID_NOT_FINISH}; \
         `None` means it did not finish within {READER_LIMIT:?}"
    );
}

/// The control: with the cache off, the same corruption is reported correctly.
/// This is what makes the two RED tests above attributable to the cache path
/// rather than to truncation in general, and it is why the green sibling file
/// runs with `rustic_no_cache = true`.
#[test]
fn with_the_cache_off_a_truncated_pack_exits_3() {
    let fx = Fixture::new();
    fx.truncate_every_pack();
    let config_dir = fx.dir.path().join("config/chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), "rustic_no_cache = true\n").unwrap();
    assert_eq!(
        fx.run_bounded(&["read", "--all-machines"]),
        Some(EXIT_DID_NOT_FINISH),
        "with the metadata cache off the reader must report the corruption as \
         exit {EXIT_DID_NOT_FINISH}"
    );
}

/// Guard: the fixture must actually be corrupting the repository it reads, so a
/// path typo cannot make the three tests above vacuous.
#[test]
fn the_fixture_really_truncates_its_repository() {
    let fx = Fixture::new();
    let before: Vec<u64> = pack_sizes(&fx.repo());
    assert!(!before.is_empty(), "the fixture wrote no packs");
    fx.truncate_every_pack();
    let after = pack_sizes(&fx.repo());
    assert_eq!(before.len(), after.len());
    for (before, after) in before.iter().zip(after.iter()) {
        assert!(
            after < before,
            "a pack was not shortened: {before} -> {after}"
        );
        assert!(
            *after > 0,
            "a pack was emptied, which is not the case under test"
        );
    }
}

fn pack_sizes(repo: &Path) -> Vec<u64> {
    let mut sizes = Vec::new();
    let mut stack = vec![repo.join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                sizes.push(path.metadata().unwrap().len());
            }
        }
    }
    sizes.sort();
    sizes
}
