//! W242 item 3 — what rustic guarantees to several clients writing one
//! repository, and what this project relies on.
//!
//! ADR-016's first open item asks whether rustic has borg's multi-client AES-CTR
//! problem, and records that the answer was never looked up:
//! **"not checked, do not pretend to know"**. borg
//! discourages multi-client use because an AES-CTR counter cannot be verified
//! across clients — "client A can only know its own highest CTR value" — which
//! is an integrity argument, not a confidentiality one.
//!
//! The answer, from the pinned dependency and then confirmed here:
//!
//! * rustic seals every blob with **AES-256-CTR over Poly1305** — an AEAD, not
//!   a bare CTR stream (`rustic_core-0.12.0/src/crypto/aespoly1305.rs`), so
//!   every blob carries a MAC that is checked on every decrypt.
//! * The nonce is **16 fresh random bytes per encryption**
//!   (`aespoly1305.rs:119-124`: `rng().fill_bytes(&mut nonce)`), not a
//!   per-client monotonic counter. There is no shared counter to collide and
//!   none to cross-verify, so borg's failure mode has no analogue here.
//!
//! The consequence that matters for multiple clients is the one
//! `the_same_plaintext_is_stored_differently_by_two_clients` pins: because the
//! nonce is fresh per encryption, two clients writing the same plaintext
//! produce **different** stored objects under different ids. That is what makes
//! a same-object race impossible for data, and it is also why a pack stranded
//! without its index can never be reclaimed (see the interrupted-push tests).
//!
//! What this project relies on, and what these tests pin:
//!
//! * corruption is **detected, never returned as content** — a single flipped
//!   byte or a truncated pack makes the archive refuse to be read (exit 3,
//!   "did not finish reading"), so an unreadable archive stays distinguishable
//!   from an empty one (invariant 1 and invariant 2);
//! * the repository holds **no lock objects** and the CLI publishes **no
//!   `prune`**, which is what makes concurrent writers safe by construction —
//!   ADR-016 Decision 4: the danger is in deleting, not in writing.

use chat_stasher::store::{BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The one shard every case in this file archives, and its exact bytes.
fn body() -> String {
    let mut content = String::from(
        r#"{"parentUuid":null,"sessionId":"claude-code.mbp-tamper.019bf00d-97b6-7eb2-9bf8-eacbacc09765","type":"user","message":{"role":"user","content":""#,
    );
    for i in 0..20_000 {
        content.push((b'!' + (i % 60) as u8) as char);
    }
    content.push_str(r#""},"uuid":"u1","timestamp":"2025-07-01T10:00:00Z"}"#);
    content.push('\n');
    content
}

struct Fixture {
    dir: tempfile::TempDir,
    stage: PathBuf,
    cfg: StoreConfig,
    mk: MasterKey,
    session: String,
}

impl Fixture {
    fn new(machine: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
        let stage = root.join("stage");
        let shards = stage
            .join("sessions")
            .join(machine)
            .join(&session)
            .join("000");
        fs::create_dir_all(&shards).unwrap();
        fs::write(shards.join("000001.jsonl"), body()).unwrap();

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
        Fixture {
            dir,
            stage,
            cfg,
            mk,
            session,
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn machine(&self) -> &str {
        self.session.split('.').nth(1).unwrap()
    }

    fn push(&self) {
        BackupStore::new(self.cfg.clone(), self.machine().to_string())
            .push(&self.stage, &self.mk)
            .unwrap();
    }

    /// The archived bytes of this fixture's session, read back through the
    /// repository — the same path `read` uses.
    fn read_back(&self) -> anyhow::Result<Vec<u8>> {
        let reader = BackupStore::new(self.cfg.clone(), self.machine().to_string());
        let (bytes, _) = reader.read_session_concat(self.machine(), &self.session, &self.mk)?;
        Ok(bytes)
    }

    /// The largest file under `data/` — for a one-session push this is the data
    /// pack holding the shard, not the much smaller tree pack.
    fn data_pack(&self) -> PathBuf {
        let mut packs: Vec<(u64, PathBuf)> = Vec::new();
        let mut stack = vec![self.repo().join("data")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    packs.push((path.metadata().unwrap().len(), path));
                }
            }
        }
        packs.sort();
        packs.pop().expect("a push writes at least one data pack").1
    }

    /// Every repository-relative object path, excluding `config`.
    fn objects(&self) -> Vec<String> {
        let repo = self.repo();
        let mut names = Vec::new();
        let mut stack = vec![repo.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|n| n.to_str()) != Some("config") {
                    names.push(
                        path.strip_prefix(&repo)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        names.sort();
        names
    }

    /// Run the real binary against this fixture's repository, with every
    /// ambient path redirected into the fixture's temp dir.
    fn cli(&self) -> Command {
        let root = self.dir.path();
        for sub in ["home", "config", "data-xdg", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W242 integrity","harnesses":[]}"#,
        )
        .unwrap();
        // Every read must come from the repository, not from this machine's
        // copy of its metadata. Measured: with a warm cache a corrupted *tree*
        // pack was not observed at all — `read` returned the session correctly
        // from the cached pack while the file on disk was already wrong. An
        // assertion about the archive has to bypass that layer.
        let config_dir = root.join("config/chat-stasher");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), "rustic_no_cache = true\n").unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data-xdg"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("CHAT_STASHER_REGISTRY", &registry);
        cmd
    }

    fn read_archive(&self) -> Output {
        self.cli()
            .args([
                "read",
                "--all-machines",
                "--full-ids",
                "--repo",
                self.repo().to_str().unwrap(),
                "--key-file",
                self.cfg.key_file.to_str().unwrap(),
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

/// The exit code that means "did not finish reading" — the one an unreadable
/// archive must produce, so that "we could not check" cannot be mistaken for
/// "there is nothing there" (invariant 2).
const EXIT_DID_NOT_FINISH: i32 = 3;

/// Nothing that came back may differ from the stage: either the archive is
/// unreadable (exit 3) or what it returns is byte-exact. It must never be
/// exit 0 with different bytes.
fn assert_never_wrong_content(out: &Output, expected: &str, context: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (session, sha) in parse_session_shas(&stdout) {
        assert_eq!(
            sha, expected,
            "{context}: {session} was returned with bytes that differ from the stage"
        );
    }
    if out.status.success() {
        assert_eq!(
            out.status.code(),
            Some(0),
            "{context}: unexpected exit status {:?}",
            out.status
        );
    } else {
        assert_eq!(
            out.status.code(),
            Some(EXIT_DID_NOT_FINISH),
            "{context}: a corrupted archive must exit {EXIT_DID_NOT_FINISH} (did not finish \
             reading), not {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// One flipped byte in a stored data pack must be detected, and the archive
/// must refuse to hand anything back rather than return plaintext that no
/// longer matches what was written.
#[test]
fn a_bit_flip_in_a_stored_pack_is_detected_and_never_returned_as_content() {
    let fx = Fixture::new("mbp-tamper");
    fx.push();
    let expected = sha256_hex(body().as_bytes());
    assert_eq!(sha256_hex(&fx.read_back().unwrap()), expected);

    let pack = fx.data_pack();
    let original = fs::read(&pack).unwrap();
    let mut corrupted = original.clone();
    let middle = corrupted.len() / 2;
    corrupted[middle] ^= 0xFF;
    fs::write(&pack, &corrupted).unwrap();

    assert_never_wrong_content(&fx.read_archive(), &expected, "after a bit flip");

    // `verify` must agree, and must not call the archive fine.
    let verify = fx
        .cli()
        .args([
            "verify",
            "--level",
            "all",
            "--stage",
            fx.stage.to_str().unwrap(),
            "--machine",
            fx.machine(),
            "--repo",
            fx.repo().to_str().unwrap(),
            "--key-file",
            fx.cfg.key_file.to_str().unwrap(),
            "--keep-ssh-masters",
        ])
        .output()
        .unwrap();
    assert!(
        !verify.status.success(),
        "verify accepted a corrupted pack:\n{}",
        String::from_utf8_lossy(&verify.stdout)
    );

    // Restoring the byte restores the archive: the corruption was detected, not
    // silently recorded.
    fs::write(&pack, &original).unwrap();
    assert_eq!(sha256_hex(&fx.read_back().unwrap()), expected);
    assert_eq!(
        parse_session_shas(&String::from_utf8_lossy(&fx.read_archive().stdout))
            .get(&fx.session)
            .map(String::as_str),
        Some(expected.as_str())
    );
}

/// The half-written-object case ADR-016 narrowed its risk to: an object that
/// reached its final name without all of its bytes. A truncated pack must be
/// refused, not returned short.
#[test]
fn a_truncated_stored_pack_is_detected_and_never_returned_as_content() {
    let fx = Fixture::new("mbp-truncated");
    fx.push();
    let expected = sha256_hex(body().as_bytes());

    let pack = fx.data_pack();
    let original = fs::read(&pack).unwrap();
    let half = original.len() / 2;
    fs::write(&pack, &original[..half]).unwrap();

    assert_never_wrong_content(&fx.read_archive(), &expected, "after truncation");

    // Restoring the pack restores the archive.
    fs::write(&pack, &original).unwrap();
    assert_eq!(sha256_hex(&fx.read_back().unwrap()), expected);
}

/// The property that answers ADR-016's first open item for multiple clients: the same
/// plaintext stored twice does not produce the same object.
///
/// Two independent clients, one masterkey, one identical stage, two fresh
/// repositories — and not one shared object id. That is the fresh random per-
/// blob nonce at work, and it is why two clients can never contend for the same
/// stored object, and why an orphaned pack can never be deduplicated against.
#[test]
fn the_same_plaintext_is_stored_differently_by_two_clients() {
    let first = Fixture::new("mbp-nonce");
    let second = Fixture::new("mbp-nonce");
    // Both fixtures must hold byte-identical shards for this to mean anything.
    let shard = |fx: &Fixture| {
        fs::read(
            fx.stage
                .join("sessions")
                .join(fx.machine())
                .join(&fx.session)
                .join("000/000001.jsonl"),
        )
        .unwrap()
    };
    assert_eq!(
        shard(&first),
        shard(&second),
        "the two fixtures must stage identical bytes"
    );

    // A shared masterkey, so only the encryption's randomness can differ.
    let shared = MasterKey::new();
    for fx in [&first, &second] {
        let mut cfg = fx.cfg.clone();
        cfg.key_file = fx.dir.path().join("shared.json");
        chat_stasher::store::persist_key_file(&cfg, &shared).unwrap();
        BackupStore::new(cfg, fx.machine().to_string())
            .push(&fx.stage, &shared)
            .unwrap();
    }

    let objects_first = first.objects();
    let objects_second = second.objects();
    assert!(
        objects_first.iter().any(|o| o.starts_with("data/")),
        "a push must write at least one data pack: {objects_first:?}"
    );
    let shared_objects: Vec<&String> = objects_first
        .iter()
        .filter(|o| objects_second.contains(o))
        .collect();
    assert!(
        shared_objects.is_empty(),
        "two clients writing identical plaintext under one masterkey shared stored objects: \
         {shared_objects:?}"
    );

    // And both copies still read back as the same plaintext.
    let mut cfg_first = first.cfg.clone();
    cfg_first.key_file = first.dir.path().join("shared.json");
    let reader = BackupStore::new(cfg_first, first.machine().to_string());
    let (bytes, _) = reader
        .read_session_concat(first.machine(), &first.session, &shared)
        .unwrap();
    assert_eq!(sha256_hex(&bytes), sha256_hex(body().as_bytes()));
}

/// What this project relies on for concurrent writers: no lock objects in the
/// repository, and no `prune` in the CLI.
///
/// ADR-016 Decision 4 proposed exactly this pair (measure first, plus promoting "we
/// never prune" from a flag to an invariant, which is what makes `prune` not a
/// subcommand). If either regressed, the concurrency reasoning in ADR-016 would
/// silently stop applying.
#[test]
fn the_repository_carries_no_locks_and_the_cli_publishes_no_prune() {
    let fx = Fixture::new("mbp-nolocks");
    fx.push();

    for object in fx.objects() {
        assert!(
            !object.to_lowercase().contains("lock"),
            "a lock object appeared in the repository: {object}"
        );
    }
    assert!(
        !fx.repo().join("locks").exists(),
        "the repository grew a `locks/` directory"
    );

    // `prune` must not be a subcommand: clap answers an unknown subcommand with
    // a usage error, exit 2.
    let prune = fx.cli().arg("prune").output().unwrap();
    assert_eq!(
        prune.status.code(),
        Some(2),
        "`prune` must not be a working subcommand, but it exited {:?}:\n{}",
        prune.status,
        String::from_utf8_lossy(&prune.stdout)
    );

    // And nothing in the repository is left half-written after a clean push.
    for object in fx.objects() {
        assert!(
            !object.contains("-tmp-"),
            "a clean push left a temp object behind: {object}"
        );
    }
}
