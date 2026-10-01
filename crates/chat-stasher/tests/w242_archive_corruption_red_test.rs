//! W242 item 3 — the reader's exit-code contract over a **truncated pack**,
//! which is exactly the half-written object ADR-016 Decision 4 narrowed its
//! risk to.
//!
//! The contract these tests assert is the project's own (CLAUDE.md invariant 2,
//! and `verify`'s documented exit codes): an archive that could not be read
//! fully must exit **3** — "did not finish reading" — so that "we could not
//! check" stays distinguishable from "there is nothing there". A crash and a
//! hang are neither: the first looks like a broken tool, the second looks like
//! a stuck one, and neither tells the user that the archive is unreadable.
//!
//! W242 wrote the first two as RED against the then-unfixed tree. Measured
//! 2026-09-29, on a repository holding one 20 KB session, with the default
//! configuration (metadata cache enabled) and a **cold** cache:
//!
//! * `read --all-machines` → exit **101**, a Rust panic:
//!   `bytes-1.12.1/src/bytes.rs:374: range end out of bounds: 3139 <= 1833`,
//!   raised at `rustic_core-0.12.0/src/backend/cache.rs:165` — the cache reads
//!   a cacheable blob (a tree) by reading the whole file and then slicing it to
//!   the range the index recorded, without checking the file is that long.
//! * `verify --level all` → did not finish within 300 s, on a repository whose
//!   whole payload is 20 KB. The panic above happens on one of
//!   `TreeStreamerOnce`'s detached workers, which dies without sending and
//!   without closing its channel, so the consumer blocks in `recv()` forever.
//!
//! Both are green now, and not because the tests were weakened: the reader
//! refuses a truncated archive *before* reading it
//! (`chat_stasher::reader_guard::require_sound_packs` asks the index whether
//! any pack it references is shorter than it records — a *missing* pack is a
//! different failure that `rustic`'s own check reports cleanly, and is
//! deliberately left to it), and a panic on the reader's own thread is reported
//! as "did not finish reading" instead of crashing. `W244-OUT.md` has the full
//! account, including the part this file cannot pin from outside the process: a
//! panic on a rustic worker thread that no guard of ours can attribute, and the
//! process-wide hook W244 tried first and removed rather than narrow.
//!
//! W242 also measured that the corruption is **invisible** while the metadata
//! cache is warm: `read` hands the session back from a cached copy of the
//! metadata pack. `the_refusal_does_not_depend_on_a_cold_cache` pins that the
//! refusal is not cache-dependent either, since a user's second run has a warm
//! cache by definition.
//!
//! The green sibling file (`w242_archive_integrity_test.rs`) runs with the
//! cache off; its assertions are about the archive's contents, not this defect.

use chat_stasher::store::{BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[path = "../src/test_support.rs"]
mod test_support;

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

    /// Truncate only the **metadata** pack, leaving the data pack whole.
    ///
    /// The fixture's content is deliberately compressible, which is what makes
    /// the tree pack the large one (the fixture's own doc says this is the pack
    /// layout it is about), so the largest pack is the metadata pack. The
    /// premise is asserted rather than assumed: if a future content change
    /// flips the order this fails here, instead of silently testing something
    /// else.
    fn truncate_metadata_pack_only(&self) {
        let paths = pack_paths(&self.repo());
        assert!(
            paths.len() >= 2,
            "the fixture wrote {} pack(s); this test needs a metadata pack and a data pack",
            paths.len()
        );
        let mut by_size: Vec<(u64, PathBuf)> = paths
            .into_iter()
            .map(|p| (p.metadata().unwrap().len(), p))
            .collect();
        by_size.sort();
        let (big, metadata) = by_size.pop().unwrap();
        let (second, _) = by_size.pop().unwrap();
        assert!(
            big > second * 2,
            "the metadata pack ({big} bytes) must dominate the data pack ({second} bytes) for \
             this test's premise to hold; the fixture's content is no longer compressible enough"
        );
        let bytes = fs::read(&metadata).unwrap();
        let keep = bytes.len() / 2;
        fs::write(&metadata, &bytes[..keep]).unwrap();
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
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&root.join("home")),
            )
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
        self.run_bounded_capturing(args).0
    }

    /// [`Fixture::run_bounded`], keeping stdout. A test that asserts on what the
    /// run *said* has to read it, and it must not become able to hang the gate
    /// by doing so: the pipe is drained on its own thread, so a child that writes
    /// more than a pipe buffer cannot block on the writer side either.
    fn run_bounded_capturing(&self, args: &[&str]) -> (Option<i32>, String) {
        let mut child: Child = self
            .cli()
            .args(args)
            .args(self.repo_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            #[allow(
                clippy::let_underscore_must_use,
                reason = "A killed child closing the pipe mid-read is an expected end of this read; what it wrote before that is still the evidence."
            )]
            let _ = out.read_to_string(&mut buf);
            buf
        });
        let deadline = Instant::now() + READER_LIMIT;
        let code = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status.code();
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            sleep(Duration::from_millis(25));
        };
        let stdout = reader.join().unwrap_or_default();
        (code, stdout)
    }
}

/// `read` must report "did not finish reading", not crash.
///
/// RED in W242 (exit 101, the `Bytes::slice` panic), green since W244.
#[test]
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
///
/// RED in W242 (no exit within the limit), green since W244.
#[test]
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

/// The control the three exit-3 tests above are read against: on a fixture
/// nobody corrupted, `verify --level all` runs **every** level and exits 0.
///
/// W244 made a level that cannot finish reading stop the run and report the
/// levels after it as not attempted (see the test below). The price of that is a
/// run that could stop early for a reason of its own, so this pins the other
/// side of it: a healthy archive still runs all three levels, and none of them
/// is skipped.
#[test]
fn verify_of_a_healthy_repository_runs_every_level_and_exits_0() {
    let fx = Fixture::new();
    let stage = fx.stage.to_string_lossy().into_owned();
    let machine = fx.machine.clone();
    let (code, stdout) = fx.run_bounded_capturing(&[
        "verify",
        "--level",
        "all",
        "--stage",
        &stage,
        "--machine",
        &machine,
    ]);
    assert_eq!(
        code,
        Some(0),
        "a healthy archive must verify with exit 0; stdout was:\n{stdout}"
    );
    for level in ["L1 structure", "L2 content", "L3 reconcile"] {
        assert!(
            stdout.contains(level),
            "`verify --level all` must run {level}; stdout was:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("NOT ATTEMPTED"),
        "a healthy archive must not skip a level; stdout was:\n{stdout}"
    );
}

/// A level that cannot finish reading stops the run, and the levels after it are
/// reported as **not attempted** — not run, and not passed.
///
/// `verify --level all` runs three levels through one store. When the first
/// cannot read the archive at all, the other two are evidence of nothing, and a
/// run that said nothing about them would read as a pass to anyone skimming the
/// summary; this is the same "did not finish" contract as the exit code, said in
/// the part of the output a person actually reads.
#[test]
fn verify_names_the_levels_it_did_not_run_instead_of_passing_them() {
    let fx = Fixture::new();
    fx.truncate_every_pack();
    let stage = fx.stage.to_string_lossy().into_owned();
    let machine = fx.machine.clone();
    let (code, stdout) = fx.run_bounded_capturing(&[
        "verify",
        "--level",
        "all",
        "--stage",
        &stage,
        "--machine",
        &machine,
    ]);
    assert_eq!(
        code,
        Some(EXIT_DID_NOT_FINISH),
        "an unreadable archive must exit {EXIT_DID_NOT_FINISH}; stdout was:\n{stdout}"
    );
    for level in ["[verify] L2 content", "[verify] L3 reconcile"] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(level))
            .unwrap_or_else(|| panic!("no `{level}` line in stdout:\n{stdout}"));
        assert!(
            line.contains("NOT ATTEMPTED"),
            "`{line}` must be reported as not attempted, not left out and not passed"
        );
    }
    assert!(
        !stdout.contains("RESULT         : OK"),
        "a run that could not read the archive must not summarise as OK; stdout was:\n{stdout}"
    );
}

/// The control: the refusal is not an artefact of the cache being enabled.
///
/// Against the unfixed tree this test was what attributed the two RED tests to
/// the cache path — with `rustic_no_cache = true` the same corruption was
/// already reported correctly (exit 3) while the cached reader crashed. Now it
/// pins the property that attribution depended on: turning the cache off must
/// change nothing, because a reader whose answer depends on a cache setting is
/// a reader whose answer depends on the machine.
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

/// The refusal must not depend on the cache being cold.
///
/// A second run has a warm metadata cache by definition, and W242 measured that
/// a warm cache makes a truncated **metadata** pack invisible to the unfixed
/// reader: the pack is served from the cache, so `read` hands the session back
/// as if the archive were intact. The audit asks the backend for the real pack
/// sizes, so it sees the truncation whether or not a cache is in the way.
#[test]
fn the_refusal_does_not_depend_on_a_cold_cache() {
    let fx = Fixture::new();
    // Reading the intact fixture once fills the metadata cache, which is the
    // configuration this test is about.
    assert_eq!(
        fx.run_bounded(&["read", "--all-machines"]),
        Some(0),
        "the intact fixture must read cleanly before it is corrupted"
    );
    fx.truncate_metadata_pack_only();
    assert_eq!(
        fx.run_bounded(&["read", "--all-machines"]),
        Some(EXIT_DID_NOT_FINISH),
        "a truncated metadata pack must be refused even when the cache still holds \
         a good copy of it"
    );
}

fn pack_sizes(repo: &Path) -> Vec<u64> {
    let mut sizes: Vec<u64> = pack_paths(repo)
        .iter()
        .map(|p| p.metadata().unwrap().len())
        .collect();
    sizes.sort();
    sizes
}

fn pack_paths(repo: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
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
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}
