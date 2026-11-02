//! W1053 regression tests — `--offline`: the read-only switch that promises
//! no remote connection, and keeps it three ways a run can be shown.
//!
//! *The promise*: with the switch, the declared destination's backend is
//! never constructed — the mirror's local backend is the whole stack — so
//! no opendal operator is built and no socket can open. The fixture that
//! proves this is the pair the ruling names: one destination repointed at a
//! blackhole (the ruling's "remote at an unreachable address", the loopback
//! accept-and-never-answer listener the W1043/W1052 fixture line uses
//! instead of TEST-NET because an outbound connect to reserved space is
//! still an outbound packet and a firewall-dependent wait — the blackhole
//! has the same "never answers" property with neither). Without the switch
//! the run reports the W1052 timeout verdict in its human wording; with the
//! switch the same read answers in seconds, byte for byte. A fix that
//! dialed "just to check" would hang that leg into the runner's own 60 s
//! kill, which is the red this fixture turns into.
//!
//! *The honesty*: a pack the copy lacks (`offline_absent_refusal`) and a
//! session whose snapshots the copy never synced (the reworded walk) are
//! both unknowns about the archive — worded so, exited `3`, never an empty
//! answer and never a claimed absence; the same session read online right
//! after is the control that proves the archive held it all along.
//!
//! *The wording*: every offline output carries `served from mirror` (the
//! ruling's own phrase for it) — `read`, `export` and
//! `verify` each print their state and served lines, and the
//! certifies-the-copy line that keeps an offline `verify` verdict from
//! reading as the destination's own.
//!
//! Everything is synthetic, local and sandboxed (W306 discipline): the
//! repository is pushed from synthetic sealed shards through the product's
//! own push, the mirror is filled by LM-2's own `sync_once`, every key is
//! freshly generated, ids/counts/digests only are asserted, and the
//! spawned children get isolated HOME/XDG envelopes with the rustic cache
//! pinned inside.
//!
//! Rollback proof: the green run needs every property above; against the
//! unfixed build (no `--offline` flag at all) each fixture reds at the
//! spawn — and the real teeth are the ones above: a seam that dialed, an
//! absence read as empty, or a served line that omitted its source would
//! fail these assertions on the fixed build too.

#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use chat_stasher::{mirror, mirror_read};
use rustic_core::repofile::MasterKey;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// Every fixture that measures a wall clock (the pair the blackhole makes,
/// and the offline runs whose swiftness is the no-connection proof)
/// serializes through this lock — the W1043/W1052 rule: two wedged runs
/// sharing cores could make a working fix look like a miss.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// --------------------------------------------------------------- fixtures

/// The fixture's destination name — the `[destinations.storagebox]` the
/// config declares and `[mirror] mirror_of` names.
const MIRROR_OF: &str = "storagebox";

/// Bind a listener on loopback that accepts connections and never answers —
/// the W1043/W1052 wedge (see the module doc for why this and not TEST-NET).
fn blackhole() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the blackhole on loopback");
    let port = listener
        .local_addr()
        .expect("read the blackhole's port")
        .port();
    std::thread::spawn(move || {
        let mut held: Vec<std::net::TcpStream> = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
        drop(held);
    });
    port
}

/// Write `nshards` sealed shards for one session under `<stage>/sessions/…`,
/// the mirror test file's shape so payloads stay comparable across suites.
fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

struct NeverStops;
impl mirror::RoundEnds for NeverStops {
    fn round_end(
        &self,
        _next: &mirror::PlannedFile,
    ) -> Result<Option<mirror::RoundEnd>, mirror::HardFailure> {
        Ok(None)
    }
}

/// One repository + mirror + sandbox, with explicit roots: the remote
/// repository (a local path, so the fixture never needs a live remote), the
/// masterkey, the sandbox every spawned child gets, and the data root the
/// mirror lives under (the same `<XDG_DATA_HOME>/chat-stasher` the CLI's
/// `decide` will resolve, so the lib-side fill and the binary-side read
/// agree on one copy).
struct Rig {
    dir: tempfile::TempDir,
    /// The pushed-to repository: a local path in the sandbox.
    repo: PathBuf,
    key: PathBuf,
    mk: MasterKey,
    /// The sandbox root every spawned child gets as its HOME/XDG envelope.
    sandbox: PathBuf,
    /// `<sandbox>/xdg-data/chat-stasher` — the data root the mirror lives
    /// under and the CLI's `decide` resolves.
    data_root: PathBuf,
    state_dir: PathBuf,
}

impl Rig {
    fn store_config(&self) -> StoreConfig {
        StoreConfig {
            repo_root: self.repo.to_string_lossy().into_owned(),
            key_file: self.key.clone(),
            connections: 1,
            options: Default::default(),
            // The lib-side fixture opens must not touch the real user cache
            // (W289); the spawned children pin their own sandboxed cache
            // through CHAT_STASHER_RUSTIC_CACHE_DIR.
            cache_dir: None,
            no_cache: true,
        }
    }

    /// Fill the mirror through LM-2's own round, so what serves the offline
    /// reads is exactly what the product fills.
    fn run_sync(&self) {
        let settings = mirror::MirrorSettings {
            enabled: true,
            mirror_of: MIRROR_OF.to_string(),
            min_free_gib: Some(0.0),
        };
        let mirror_root = mirror::mirror_dir(&self.data_root, MIRROR_OF);
        let min_free_bytes = settings.min_free_bytes(&mirror_root);
        mirror::sync_once(&mirror::SyncRequest {
            settings: &settings,
            remote: &self.store_config(),
            data_root: &self.data_root,
            state_dir: &self.state_dir,
            master_key: &self.mk,
            now_unix: 1_800_000_000,
            min_free_bytes,
            round_ends: &NeverStops,
        })
        .expect("the fixture's sync round completes");
    }

    /// The same `decide` the CLI runs, with a config that names the
    /// destination and the section — the lib-side check that the copy the
    /// binary will serve from is on the exact path it resolves.
    fn serving_reads(&self) -> std::sync::Arc<mirror_read::MirrorReads> {
        let mut config = chat_stasher::config::Config::default();
        config.destinations.insert(
            MIRROR_OF.to_string(),
            chat_stasher::config::DestinationConfig {
                repo: Some(self.repo.to_string_lossy().into_owned()),
                ..Default::default()
            },
        );
        config.mirror = Some(chat_stasher::config::MirrorSectionConfig {
            enabled: Some(true),
            mirror_of: Some(MIRROR_OF.to_string()),
            min_free_gib: None,
        });
        match mirror_read::decide(&config, &self.store_config(), &self.data_root) {
            mirror_read::MirrorReadDecision::Serving(reads) => reads,
            other => panic!("the fixture's synced mirror must serve, got {other:?}"),
        }
    }

    /// Remove every pack file from the copy (metadata — config, keys,
    /// index, snapshots — stays, so opens and listings still answer): the
    /// "mirror lacks this file" shape for the refusal leg.
    fn strip_packs_from_mirror(&self) {
        let data = mirror::mirror_dir(&self.data_root, MIRROR_OF).join("data");
        for entry in fs::read_dir(&data).unwrap() {
            let shard_dir = entry.unwrap().path();
            for file in fs::read_dir(&shard_dir).unwrap() {
                fs::remove_file(file.unwrap().path()).unwrap();
            }
        }
    }

    /// Write the sandboxed config: the local repository (the
    /// pre-repointing shape), or the S3-shaped unreachable one when
    /// `unreachable_endpoint` is given — the W1052 fixture's atom pair
    /// (`retry = off`, synthetic credentials), plus the `[mirror]` section
    /// that makes the copy serve.
    fn plant_config(&self, unreachable_endpoint: Option<String>) {
        let config_dir = self.sandbox.join("xdg-config").join("chat-stasher");
        fs::create_dir_all(&config_dir).expect("create the isolated config dir");
        let mut config = String::new();
        config.push_str("machine = \"machine-a\"\n\n");
        match unreachable_endpoint {
            None => {
                config.push_str(&format!(
                    "[destinations.{MIRROR_OF}]\nrepo = \"{}\"\nkey_file = \"{}\"\nconnections = 2\n",
                    self.repo.display(),
                    self.key.display(),
                ));
            }
            Some(endpoint) => {
                config.push_str(&format!(
                    "[destinations.{MIRROR_OF}]\nrepo = \"opendal:s3\"\nkey_file = \"{}\"\n\
                     connections = 2\n\n[destinations.{MIRROR_OF}.options]\nbucket = \
                     \"chat-stasher\"\nendpoint = \"{endpoint}\"\nregion = \"auto\"\nroot = \
                     \"/synthetic/v1\"\naccess_key_id = \"synthetic-access-key-id\"\n\
                     secret_access_key = \"synthetic-secret-access-key\"\n\
                     disable_config_load = \"true\"\nretry = \"off\"\n",
                    self.key.display(),
                ));
            }
        }
        config.push_str(&format!(
            "\n[mirror]\nenabled = true\nmirror_of = \"{MIRROR_OF}\"\n"
        ));
        fs::write(config_dir.join("config.toml"), config).expect("write the fixture config");
    }
}

/// The full rig: sandbox + repository pushed from a synthetic stage + a
/// mirror synced from it + the same stage (which L3 and export read back),
/// all under one temp root.
fn build_rig() -> Rig {
    let dir = tempfile::tempdir().expect("create the rig's root");
    let sandbox = dir.path().join("sandbox");
    fs::create_dir_all(&sandbox).expect("create the sandbox");
    let data_root = sandbox.join("xdg-data").join("chat-stasher");
    let state_dir = sandbox.join("xdg-state").join("chat-stasher");
    let rig = Rig {
        repo: dir.path().join("remote-repo"),
        key: sandbox.join("masterkey-storagebox.json"),
        mk: MasterKey::new(),
        sandbox,
        data_root,
        state_dir,
        dir,
    };

    // Two sessions for machine-a: session-a1 exists before the mirror's
    // sync (the offline pair reads it), session-a2 is pushed only after —
    // the copy never learns about it, which is the "missing entry" leg.
    let stage = rig.dir.path().join("stage");
    write_session(&stage, "machine-a", "claude-code.machine-a.sess-a0001", 2);
    let cfg = rig.store_config();
    store::persist_key_file(&cfg, &rig.mk).expect("persist the synthetic key");
    BackupStore::new(cfg, "machine-a".to_string())
        .push(&stage, &rig.mk)
        .expect("push the fixture's first snapshot");
    rig.run_sync();
    // The snapshot the copy does not have: a second session pushed after
    // the sync, so the remote holds it and the copy cannot.
    write_session(&stage, "machine-a", "claude-code.machine-a.sess-a0002", 3);
    BackupStore::new(rig.store_config(), "machine-a".to_string())
        .push(&stage, &rig.mk)
        .expect("push the fixture's second snapshot");
    rig.plant_config(None);
    rig
}

// ----------------------------------------------------------- run machinery

/// One command in a fully sandboxed HOME (the W1052 harness shape), with
/// every read-step budget set to `budget_secs` when given.
fn command(rig: &Rig, args: &[&str], budget_secs: Option<u64>) -> Command {
    let home = rig.sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let no_tools = rig.sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).expect("create empty PATH directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(args)
        .env("PATH", &no_tools)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", rig.sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", rig.sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", rig.sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", rig.sandbox.join("xdg-cache"))
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env_remove("CHAT_STASHER_READ_STEP_BUDGET_SECS");
    if let Some(budget_secs) = budget_secs {
        command.env(
            chat_stasher::readstep::READ_STEP_BUDGET_ENV,
            budget_secs.to_string(),
        );
    }
    command
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    command
}

/// One run's evidence, polled and killed at a hard deadline rather than
/// waited on (the W1052 rule: a fixture that can hang must turn the hang
/// into a red exit, never a stuck CI job).
struct Run {
    code: Option<i32>,
    text: String,
    elapsed: Duration,
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "the reads and the kill below are best-effort by design: a child killed at the hard \
              deadline leaves a truncated stream whose bytes are the evidence, and `None` — not \
              an unwrap — is how a wedged run must report itself"
)]
fn run_bounded(mut command: Command) -> Run {
    const HARD_DEADLINE: Duration = Duration::from_secs(60);
    use std::io::Read;
    use std::process::Stdio;
    let started = std::time::Instant::now();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the fixture's command");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() > HARD_DEADLINE {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("waiting for the fixture's command: {error}"),
        }
    };
    let stdout = stdout_reader.join().expect("read stdout");
    let stderr = stderr_reader.join().expect("read stderr");
    Run {
        code: status.map(|status| status.code().unwrap_or(-1)),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ),
        elapsed: started.elapsed(),
    }
}

/// The `concat len …  sha256=<hex>` of a read's report line, the one digest
/// every parity leg compares.
fn concat_sha(text: &str) -> String {
    text.lines()
        .find(|line| line.starts_with("[read] concat len"))
        .and_then(|line| {
            line.split_once("sha256=")
                .map(|(_, digest)| digest.trim().to_string())
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the pair

/// The ruling's ①, in full: the destination points at an address that never
/// answers, and the same session reads twice. Without the switch the run
/// reports the W1052 timeout verdict — the named step, the budget, the
/// unknown-not-a-measurement sentence, exit 3 — exactly as it must with a
/// mirror configured (the online path is W1052's, untouched). With the
/// switch the run never constructs the destination's backend, so the
/// blackhole cannot be dialed: the read answers fast, exits 0, prints
/// `served from mirror`, and the digest is the same as the online read of
/// the same session over the local repository — which is ②'s own parity
/// made one leg of the unreachable pair.
#[test]
fn the_offline_pair_over_an_unreachable_destination() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();

    // ② first, against the still-local destination: the online digest the
    // unreachable pair must reproduce.
    let online = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0001",
        ],
        None,
    ));
    assert_eq!(
        online.code,
        Some(0),
        "the online local read is the digest baseline: {}",
        online.text
    );
    let online_sha = concat_sha(&online.text);
    assert_eq!(
        online_sha.len(),
        64,
        "the digest must have parsed: {}",
        online.text
    );
    assert!(
        online
            .text
            .contains("[read] mirror         : first `storagebox`"),
        "the online mirror state line keeps its exact wording: {}",
        online.text
    );

    // The destination goes unreachable: the same declared destination,
    // repointed at a blackhole (the ruling's unreachable address), while
    // the mirror on disk keeps holding the synced bytes.
    let port = blackhole();
    rig.plant_config(Some(format!("http://127.0.0.1:{port}")));

    // Without the switch: W1052's verdict, human and exit 3, never a hang.
    let budget: u64 = 2;
    let wedged = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0001",
        ],
        Some(budget),
    ));
    assert!(
        wedged.elapsed < Duration::from_secs(budget + 20),
        "the online wedge is bounded by its step budget; waited {:?}",
        wedged.elapsed
    );
    assert_eq!(
        wedged.code,
        Some(3),
        "the wedged destination exits in the did-not-finish family: {}",
        wedged.text
    );
    assert!(
        wedged
            .text
            .contains("read: 🔴 reading the session from the archive did not answer within 2s"),
        "the W1052 timeout verdict keeps its wording with a mirror configured: {}",
        wedged.text
    );
    assert!(
        wedged.text.contains("read: INCOMPLETE exit_code=3"),
        "one INCOMPLETE line with the exit code it claims: {}",
        wedged.text
    );
    assert!(
        wedged.text.contains("This is unknown, not a measurement"),
        "a deadline is an unknown, never a measurement: {}",
        wedged.text
    );
    assert!(
        !wedged.text.contains("[read] concat len"),
        "no report line for a session that was not read: {}",
        wedged.text
    );

    // With the switch: no remote connection exists to dial — the read
    // serves from the mirror, prints it, and matches the online digest.
    let offline = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0001",
            "--offline",
        ],
        None,
    ));
    assert!(
        offline.elapsed < Duration::from_secs(30),
        "an offline read over the local copy answers fast — a run that dialed \
         the blackhole would hang into the harness's kill; waited {:?}",
        offline.elapsed
    );
    assert_eq!(
        offline.code,
        Some(0),
        "the offline switch reads through the unreachable destination: {}",
        offline.text
    );
    assert_eq!(
        concat_sha(&offline.text),
        online_sha,
        "the offline digest is the online digest, byte for byte: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("[read] mirror         : served from mirror `storagebox`"),
        "the offline state line states its source: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("[read] mirror served  : served from mirror `storagebox`"),
        "the served line states it again with what moved: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("nothing from the remote (--offline; this run opened no remote connection)"),
        "the served line words the remote half as the switch's promise, not a measured zero: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("[read] ssh reap       : N/A (--offline; this run opened no ssh master)"),
        "the reap line says why there is nothing to reap: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("[read] archive        : reading session "),
        "the archive trace runs before the read, named: {}",
        offline.text
    );
    assert!(
        offline.text.contains(
            " served from mirror, cumulatively, newest snapshot first — the \
                       destination is not contacted"
        ),
        "the archive trace names its source before it runs (W1052 compatibility): {}",
        offline.text
    );
}

/// The missing entry, pack shape: with the copy's packs gone, the walk fails
/// closed at the first control read — the honest
/// "the read fails closed rather than answer from a policy it cannot
/// establish" sentence, exit 3 — and never an empty report and never a
/// claimed absence of the session (the ruling's ③). The copy-specific
/// per-file refusal itself (the not-contacted, fix-with-`mirror-sync`
/// wording) is pinned at seam level in `mirror_read`'s unit tests; here the
/// pin is what the CLI may honestly report: did-not-finish, nothing
/// presented as an answer, nothing claimed about the archive.
#[test]
fn packs_the_copy_lacks_stop_the_read_did_not_finish_never_empty() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();
    let port = blackhole();
    rig.plant_config(Some(format!("http://127.0.0.1:{port}")));
    rig.strip_packs_from_mirror();

    let run = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0001",
            "--offline",
        ],
        Some(30),
    ));
    assert_eq!(
        run.code,
        Some(3),
        "a copy that cannot serve the read is did-not-finish, never an empty report: {}",
        run.text
    );
    assert!(
        run.text.contains("this read fails closed"),
        "the fail-closed wording states that no answer was established: {}",
        run.text
    );
    assert!(
        !run.text.contains("[read] concat len"),
        "no report line may present a partial body as a session: {}",
        run.text
    );
    assert!(
        !run.text.contains("holds no shards in any of"),
        "the read never claims it walked everything and found nothing: {}",
        run.text
    );
}

/// The missing entry, session shape: a session pushed after the copy's
/// last sync is unknown offline — worded as unknown, exit 3 — while the
/// same session reads fine online right after, which is the control that
/// proves the archive held it all along and the offline answer was honesty,
/// not emptiness.
#[test]
fn a_session_the_copy_never_synced_is_unknown_offline_and_present_online() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();

    // Offline: the walk reads every snapshot the copy holds, finds none with
    // the session, and must not claim the online proof ("all of them were
    // read" over this repository) — the copy's walk proves nothing about
    // snapshots it never synced.
    let offline = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0002",
            "--offline",
        ],
        None,
    ));
    assert_eq!(
        offline.code,
        Some(3),
        "the copy's absence is an unknown, exited in the did-not-finish family: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("the local mirror copy of `storagebox` holds (--offline"),
        "the walk names whose snapshots were read: {}",
        offline.text
    );
    assert!(
        offline.text.contains("the destination was not contacted"),
        "and what was not: {}",
        offline.text
    );
    assert!(
        offline
            .text
            .contains("so this is unknown, not a proof the session is absent"),
        "the refusal words itself as an unknown, never a claimed absence: {}",
        offline.text
    );
    assert!(
        !offline.text.contains("[read] concat len"),
        "an unknown session prints no report line: {}",
        offline.text
    );

    // Online, over the very same destination: the session is there, the
    // mirror's fallback fetches what the copy lacks, and the report prints.
    // This is the red/green control for ③ — against a build that answered
    // the offline leg as empty, this run right here is what the offline
    // answer would have lied about.
    let online = run_bounded(command(
        &rig,
        &[
            "read",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--session",
            "claude-code.machine-a.sess-a0002",
        ],
        None,
    ));
    assert_eq!(
        online.code,
        Some(0),
        "the control reads the same session online: {}",
        online.text
    );
    assert_eq!(
        concat_sha(&online.text).len(),
        64,
        "the control's report is the proof the session was never absent: {}",
        online.text
    );
}

/// `verify --offline` certifies the copy, never the destination: over the
/// blackhole-pointed destination an L2 run answers from the copy and exits
/// OK with the certifies-the-copy line and the served line — a run that
/// dialed the destination would hang into the harness's kill, which is the
/// red a broken offline build would turn into.
#[test]
fn verify_offline_certifies_the_copy_over_an_unreachable_destination() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();
    let copy = mirror::mirror_dir(&rig.data_root, MIRROR_OF);
    assert!(
        copy.join("config").is_file(),
        "the copy must be filled first"
    );

    let port = blackhole();
    rig.plant_config(Some(format!("http://127.0.0.1:{port}")));
    let run = run_bounded(command(
        &rig,
        &[
            "verify",
            "--destination",
            MIRROR_OF,
            "--offline",
            "--level",
            "l2",
        ],
        None,
    ));
    assert!(
        run.elapsed < Duration::from_secs(45),
        "an offline verify is a local walk; one that dialed the blackhole would have hung; \
         waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(0),
        "L2 over the verified copy is a self-consistent pass: {}",
        run.text
    );
    assert!(
        run.text.contains("[verify] RESULT         : OK"),
        "the certifies-the-copy verdict is stated: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "served from mirror `storagebox` — verifying the local mirror copy only, so the \
             verdict certifies the copy, never the destination's repository"
        ),
        "the mirror line keeps a copy verdict from passing as the destination's own: {}",
        run.text
    );
    assert!(
        run.text
            .contains("[verify] mirror served  : served from mirror `storagebox`"),
        "the served line states the ruling's phrase: {}",
        run.text
    );
}

/// `verify --offline --level l3`: a session the stage expects that the copy
/// does not hold is an unknown about the archive — a `NOT IN THE LOCAL
/// COPY` row and an INCOMPLETE verdict, exit 3 — never `MISSING IN
/// ARCHIVE`, because the destination was not asked.
#[test]
fn verify_offline_l3_words_a_missing_session_as_unknown() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();
    let stage = rig.dir.path().join("stage");
    let port = blackhole();
    rig.plant_config(Some(format!("http://127.0.0.1:{port}")));

    let run = run_bounded(command(
        &rig,
        &[
            "verify",
            "--destination",
            MIRROR_OF,
            "--offline",
            "--level",
            "l3",
            "--stage",
            stage.to_str().expect("utf-8 stage path"),
        ],
        None,
    ));
    assert_eq!(
        run.code,
        Some(3),
        "an L3 the copy could not answer in full is INCOMPLETE, exit 3: {}",
        run.text
    );
    assert!(
        run.text
            .contains("NOT IN THE LOCAL COPY (--offline; unknown whether the archive holds it"),
        "the row words the offline unknown: {}",
        run.text
    );
    assert!(
        !run.text.contains("MISSING IN ARCHIVE"),
        "an offline L3 never claims the archive lacks what the copy never synced: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "L3 verdict       : INCOMPLETE (1 expected row(s) not in the local mirror copy"
        ),
        "the L3 verdict counts the copy's unknowns: {}",
        run.text
    );
    assert!(
        run.text
            .contains("[verify] RESULT         : INCOMPLETE (1 level(s) unreadable)"),
        "and the run's verdict follows: {}",
        run.text
    );
}

/// `export --offline` writes exactly the sessions the copy can see, from
/// the copy, byte for byte: the served line states the source, the exported
/// file of the synced session matches the online export's sha256, and over
/// the unreachable destination the run never dials (fast, exit 0). The
/// selection the copy cannot see is a fact the served line scopes.
#[test]
fn export_offline_serves_the_copy_byte_identically() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rig = build_rig();

    // The online baseline, over the still-local destination.
    let online_out = rig.dir.path().join("export-online");
    let online = run_bounded(command(
        &rig,
        &[
            "export",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--out",
            online_out.to_str().expect("utf-8 out path"),
        ],
        None,
    ));
    assert_eq!(
        online.code,
        Some(0),
        "the online export is the byte-for-byte baseline: {}",
        online.text
    );
    let online_entry = manifest_entry(&online_out, "claude-code.machine-a.sess-a0001");

    let port = blackhole();
    rig.plant_config(Some(format!("http://127.0.0.1:{port}")));
    let offline_out = rig.dir.path().join("export-offline");
    let run = run_bounded(command(
        &rig,
        &[
            "export",
            "--destination",
            MIRROR_OF,
            "--machine",
            "machine-a",
            "--offline",
            "--out",
            offline_out.to_str().expect("utf-8 out path"),
        ],
        None,
    ));
    assert!(
        run.elapsed < Duration::from_secs(45),
        "an offline export is a local walk; one that dialed the blackhole would have hung; \
         waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(0),
        "the copy answers the export the archive selection could see: {}",
        run.text
    );
    assert!(
        run.text
            .contains("[export] mirror         : served from mirror `storagebox`"),
        "the offline state line is on stderr before the plan: {}",
        run.text
    );
    assert!(
        run.text
            .contains("[export] mirror served  : served from mirror `storagebox`"),
        "the served line closes with the phrase: {}",
        run.text
    );
    let offline_entry = manifest_entry(&offline_out, "claude-code.machine-a.sess-a0001");
    assert_eq!(
        offline_entry.sha256, online_entry.sha256,
        "the exported session is byte-identical, whichever disk served it"
    );
    assert_eq!(
        offline_entry.written, online_entry.written,
        "and its byte count is the same"
    );
}

/// One manifest entry: the per-session row `export` writes (sha256 of the
/// written file and bytes written), parsed with the same JSON the tool
/// itself serialises.
struct ManifestEntry {
    sha256: String,
    written: u64,
}

/// The manifest row for `session`, by walking the exported `manifest.json`'s
/// `sessions` array (keeping the read inside the test, opaque and bounded:
/// ids, counts and digests only — no conversation text is inspected).
fn manifest_entry(out: &Path, session: &str) -> ManifestEntry {
    let manifest: serde_json::Value = serde_json::from_reader(
        fs::File::open(out.join("manifest.json")).expect("the run wrote its manifest"),
    )
    .expect("the manifest parses");
    let rows = manifest
        .as_object()
        .expect("the manifest is an object")
        .get("sessions")
        .and_then(|s| s.as_array())
        .expect("the manifest carries its sessions array");
    for row in rows {
        if row.get("session_id").and_then(|id| id.as_str()) == Some(session) {
            return ManifestEntry {
                sha256: row
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                written: row.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
            };
        }
    }
    panic!("no manifest row for {session} under {}", out.display());
}

/// The decision's offline capability is the contract the fixtures rest on:
/// the lib-side check that this rig's synced copy is exactly what the
/// binary's `decide` will call `Serving`, and that the states which cannot
/// serve offline are the ones the unit tests refuse (the seam is the same
/// object both sides).
#[test]
fn the_rig_copy_serves_offline_through_the_real_decision() {
    let rig = build_rig();
    let reads = rig.serving_reads();
    assert_eq!(reads.mirror_of, MIRROR_OF);
    assert_eq!(
        reads.mirror_root,
        mirror::mirror_dir(&rig.data_root, MIRROR_OF)
    );
    let mut config = chat_stasher::config::Config::default();
    config.mirror = Some(chat_stasher::config::MirrorSectionConfig {
        enabled: Some(true),
        mirror_of: Some(MIRROR_OF.to_string()),
        min_free_gib: None,
    });
    let no_destinations = mirror_read::decide(&config, &rig.store_config(), &rig.data_root);
    assert!(
        !no_destinations.offline_capable(),
        "a section naming no declared destination cannot serve an offline run"
    );
    assert!(no_destinations.offline_refusal().is_some());
}
