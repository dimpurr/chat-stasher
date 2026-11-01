//! W1052 regression tests — `read` must always name the step it is on and
//! reach an exit, even when the destination (or a local file) never answers.
//!
//! Rollback proof: before this change, one wedged step froze the whole
//! command after its five pre-flight lines — the observed incident hung a
//! 1373-byte session read for 120 s with the length+sha256 report never
//! printed and no exit code, because `read_session_concat_with_view` walks
//! a backend whose calls open connections with no timeout of their own.
//! The fix is the DOCTOR-HANG one (W1043), a command over: every step prints
//! what it is doing before it runs, and every step that can block on
//! something this machine does not control runs behind its own deadline.
//! Each fixture below asserts the deadline machinery's observable answer end
//! to end, so reverting the wiring makes the fixture red: with the fix
//! removed, these exact spawns hang, and the hard deadline each fixture
//! enforces turns that hang into a kill the exit-code assertions fail on —
//! the same teeth `w1043_doctor_hang_test` ships.
//!
//! The wedge is deterministic and machine-local: a TCP listener on loopback
//! that accepts the connection and then never answers it — the shape the
//! observed production hang had (the connection opened; the read never
//! ended) — pointed at by an S3-shaped destination in a sandboxed HOME; the
//! local file wedges are FIFOs nobody ever opens for writing, so the
//! reader's `open()` never returns (`std` has no other file shape whose open
//! blocks). No real remote, real credential or real session body is
//! involved; every key file below is freshly generated, every name, count
//! and byte is synthetic, and nothing the spawned child reads can be
//! conversation text (a `read` prints ids, counts, lengths and digests
//! only). TEST-NET-shaped unreachability is *not* used: an outbound connect
//! to reserved space is still an outbound packet and a firewall-dependent
//! wait — the blackhole listener is the same "never answers" property with
//! neither.

#[path = "../src/test_support.rs"]
mod test_support;

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Every run below measures a wall-clock budget, so two fixtures running at
/// once on the same cores could make a working fix look like a miss: they
/// serialize through this lock (`w1043_doctor_hang_test`'s rule, copied for
/// the same reason — the readers of one slow resource serialize too).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Bind a listener on loopback that accepts connections and then never
/// answers one — a connect-succeeds-then-reads-forever wedge, the shape the
/// observed hang had. Accepted sockets are held open (dropping one would
/// send an RST and turn the wedge into a quick refusal, the failure mode
/// this fixture must not be satisfied by).
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
                Ok(stream) => {
                    // Set no read/write timeout: the wedge is the point.
                    held.push(stream);
                }
                Err(_) => break,
            }
        }
        drop(held);
    });
    port
}

/// The FIFO shape the local-file wedges use: a file nobody ever opens for
/// writing, so the reader's `open()` never returns. Unix-only mechanism,
/// stated per CLAUDE.md's cfg rule because it is the property that is
/// platform-bound, not the fix — `std` cannot create a writer-less FIFO on
/// Windows, and the timeouts and traces under test are platform-neutral
/// control flow the unix CI cells exercise on every push.
#[cfg(unix)]
fn wedged_fifo(at: &Path) {
    let mkfifo = Command::new("mkfifo")
        .arg(at)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "run mkfifo for the wedged FIFO at {}: {error}",
                at.display()
            )
        });
    assert!(
        mkfifo.success(),
        "mkfifo must create the wedged FIFO at {}",
        at.display()
    );
}

/// Write a real repository key file at `path` for a config that must load —
/// the archive step's wedge only happens once the key read answered, so
/// every fixture that wedges later needs a key file that is honest, and no
/// test ever reads a key that belongs to any real machine.
fn plant_key_file(key: &Path) {
    let cfg = chat_stasher::store::StoreConfig {
        repo_root: "opendal:s3".to_string(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: Default::default(),
        cache_dir: None,
        no_cache: false,
    };
    let mk = rustic_core::repofile::MasterKey::new();
    chat_stasher::store::persist_key_file(&cfg, &mk).expect("persist the synthetic key");
}

/// Write the sandboxed config naming one destination, `blackhole`-shaped
/// unless `endpoint` says otherwise, with its key file at `key`.
fn plant_destinated_config(
    sandbox: &Path,
    endpoint: &str,
    key: &Path,
    destinations: bool,
) -> std::io::Result<()> {
    let config_dir = sandbox.join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir)?;
    let mut config = fs::File::create(config_dir.join("config.toml"))?;
    if destinations {
        writeln!(
            config,
            r#"machine = "w1052-fixture"

[destinations.blackhole]
repo = "opendal:s3"
key_file = "{key}"
connections = 2

[destinations.blackhole.options]
bucket = "chat-stasher"
endpoint = "{endpoint}"
region = "auto"
root = "/synthetic/v1"
access_key_id = "synthetic-access-key-id"
secret_access_key = "synthetic-secret-access-key"
disable_config_load = "true"
"#,
            key = key.display(),
        )?;
    } else {
        writeln!(config, r#"machine = "w1052-fixture""#)?;
    }
    Ok(())
}

/// One `read` command in a fully sandboxed HOME, with every read-step budget
/// set to `budget_secs`, or with the production budgets when it is `None`
/// (the override is the fixtures' — `Some(2)` would otherwise wait out the
/// 600 s archive budget; one *control* fixture deliberately runs
/// unoverridden so a measured error reports itself the way a real run
/// would).
fn read_command(sandbox: &Path, args: &[&str], budget_secs: Option<u64>) -> Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let no_tools = sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).expect("create empty PATH directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(args)
        .env("PATH", &no_tools)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
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

/// One run's whole evidence package, in the shape the assertions read.
///
/// The child is polled and killed at a hard deadline rather than waited on:
/// this fixture exists to catch the very hang it simulates, and a fixture
/// that could hang with it would turn the regression it guards into a
/// stuck CI job instead of a red test. `code: None` therefore means "the
/// child was still alive at the deadline" — every exit-code assertion fails
/// loudly on that, which is exactly what a reverted W1052 must look like.
struct Run {
    code: Option<i32>,
    text: String,
    elapsed: Duration,
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "the reads and the kill below are best-effort by design: a child that was killed \
              at the hard deadline leaves a truncated stream whose bytes are the evidence, and \
              reaping a process that may already have exited is exactly the failure whose \
              verdict `None` — not an `unwrap` — encodes"
)]
fn run_bounded(mut command: Command) -> Run {
    const HARD_DEADLINE: Duration = Duration::from_secs(60);
    use std::io::Read;
    use std::process::Stdio;
    let started = Instant::now();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn read");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Drain on threads of their own: a child that fills an unread pipe
    // buffer would block on writing, not on wedging, and the fixture is
    // about the wedge.
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
            Err(error) => panic!("waiting for read: {error}"),
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

/// The returning story, single session: a destination that holds the
/// connection and never answers gets its archive read abandoned at the
/// budget, **after** the pre-flight lines the observed incident had already
/// printed — the five lines stay, byte for byte — and the run ends in the
/// timeout's honest wording and exit 3, never a silent hang and never a
/// report line for a session that was not read.
#[test]
fn read_returns_within_n_seconds_when_the_destination_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let port = blackhole();
    let key = sandbox.path().join("key.json");
    plant_key_file(&key);
    plant_destinated_config(
        sandbox.path(),
        &format!("http://127.0.0.1:{port}"),
        &key,
        true,
    )
    .expect("write the destination config");
    let budget: u64 = 2;
    let run = run_bounded(read_command(
        sandbox.path(),
        &[
            "read",
            "--destination",
            "blackhole",
            "--session",
            "w1052.synthetic-session",
        ],
        Some(budget),
    ));
    // Exactly one step can consume its full budget on this fixture (the
    // archive read; config, identity, the local tiers and the key file all
    // answer in milliseconds), so the bound is that budget plus the
    // empty-sandbox locals and slack — and far under `run_bounded`'s 60 s
    // kill, which is how a reverted fix fails this test instead of hanging
    // CI.
    assert!(
        run.elapsed < Duration::from_secs(budget + 20),
        "read must bound its run when the destination wedges; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the deadline's exit is 3, not a hang: {}",
        run.text
    );
    // The five pre-flight lines the observed incident printed still print,
    // before the wedge: a hang that has already said which machine, repo, and
    // cache it is using is a diagnosable one.
    assert!(
        run.text
            .contains("[read] config         : reading the configuration file")
            && run
                .text
                .contains("[read] identity       : resolving this machine's archive partition")
            && run.text.contains("[read] body cache     : ")
            && run.text.contains("[read] machine        : w1052-fixture")
            && run.text.contains("[read] repo           : opendal:s3")
            && run.text.contains("[read] ssh reap       : ON"),
        "the trace and pre-flight lines must name every step before it runs: {}",
        run.text
    );
    // The trace names the archive step as the thing that wedged…
    assert!(
        run.text.contains("[read] archive        : reading session"),
        "the trace must name the archive step before it wedges: {}",
        run.text
    );
    // …and the timeout words it: unknown, with the budget, and never as the
    // verdicts a measured refusal or a missing session owns.
    assert!(
        run.text
            .contains("read: 🔴 reading the session from the archive did not answer within 2s"),
        "the verdict names the step and its budget: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "read: INCOMPLETE exit_code=3 — reading the session from the archive did not answer \
             within 2s"
        ),
        "one INCOMPLETE line with the exit code the sentence claims: {}",
        run.text
    );
    assert!(
        run.text.contains("This is unknown, not a measurement"),
        "a deadline is an unknown, never a measurement: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "lift every read budget at once with CHAT_STASHER_READ_STEP_BUDGET_SECS=<seconds>"
        ),
        "the verdict names the knob that lifts it: {}",
        run.text
    );
    assert!(
        !run.text.contains("[read] concat len"),
        "no report line for a session that was not read: {}",
        run.text
    );
    assert!(
        !run.text.contains("holds no shards in any of the snapshots"),
        "a timed-out read is not a proof the session is absent: {}",
        run.text
    );
}

/// The bulk path the single-session one shares: `--all-machines` wedges in
/// the same backend walk, and its walk — which exists to read *everything* —
/// is worded as stopping part-way, never as an empty archive.
#[test]
fn all_machines_read_bounds_the_same_wedge() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let port = blackhole();
    let key = sandbox.path().join("key.json");
    plant_key_file(&key);
    plant_destinated_config(
        sandbox.path(),
        &format!("http://127.0.0.1:{port}"),
        &key,
        true,
    )
    .expect("write the destination config");
    let budget: u64 = 2;
    let run = run_bounded(read_command(
        sandbox.path(),
        &["read", "--destination", "blackhole", "--all-machines"],
        Some(budget),
    ));
    assert!(
        run.elapsed < Duration::from_secs(budget + 20),
        "the bulk read is bounded too; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the bulk deadline's exit is 3: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "[read] archive        : reading every snapshot of every machine (downloads and hashes \
             every shard)"
        ),
        "the bulk trace names its walk before it wedges: {}",
        run.text
    );
    assert!(
        run.text
            .contains("read: 🔴 the all-machines archive read did not answer within 2s"),
        "the bulk verdict names its step and budget: {}",
        run.text
    );
    assert!(
        run.text
            .contains("every machine's session counts are unknown"),
        "a partial walk is an unknown, never an emptiness: {}",
        run.text
    );
}

/// The config step, wedged the way `w1043_doctor_hang_test` wedges it: a
/// config file whose reader never answers. `read`'s config step is the
/// *first* thing it does, so this pins that the wedge is bounded where the
/// unfixed command hung with zero output at all.
#[test]
#[cfg(unix)]
fn read_bounds_a_configuration_file_that_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let config_dir = sandbox.path().join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir).expect("create the isolated config dir");
    wedged_fifo(&config_dir.join("config.toml"));
    let budget: u64 = 2;
    let run = run_bounded(read_command(
        sandbox.path(),
        &["read", "--session", "w1052.synthetic-session"],
        Some(budget),
    ));
    assert!(
        run.elapsed < Duration::from_secs(budget + 10),
        "the config step is bounded first; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the config deadline's exit is 3, not a hang: {}",
        run.text
    );
    assert!(
        run.text
            .contains("[read] config         : reading the configuration file"),
        "the trace names the config step before it wedges: {}",
        run.text
    );
    assert!(
        run.text
            .contains("🔴 reading the configuration file did not answer within 2s"),
        "the verdict words the unknown: {}",
        run.text
    );
    assert!(
        !run.text.contains("[read] identity"),
        "no step after the config ran — the wedge stopped the run at the first step, which is \
         the bounded behaviour this fixture pins: {}",
        run.text
    );
}

/// The key step, wedged the same way: a config that answers, a key file that
/// never does. The machine and destination are known and printed before the
/// wedge, so the verdict's consequence is exact: nothing in the archive
/// could be decrypted, and that is why nothing was read.
#[test]
#[cfg(unix)]
fn read_bounds_a_master_key_file_that_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let port = blackhole();
    let key = sandbox.path().join("key.json");
    // Plant the *config* first: the key path itself is the wedge, so the
    // config must be readable and the destination resolvable for the run to
    // reach the key step at all.
    plant_destinated_config(
        sandbox.path(),
        &format!("http://127.0.0.1:{port}"),
        &key,
        true,
    )
    .expect("write the destination config");
    wedged_fifo(&key);
    let budget: u64 = 2;
    let run = run_bounded(read_command(
        sandbox.path(),
        &[
            "read",
            "--destination",
            "blackhole",
            "--session",
            "w1052.synthetic-session",
        ],
        Some(budget),
    ));
    assert!(
        run.elapsed < Duration::from_secs(budget + 20),
        "the key step is bounded; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the key deadline's exit is 3, not a hang: {}",
        run.text
    );
    // The steps before the wedge printed — the config, the identity and the
    // tiers the run consults before it needs the key — and the wedge's
    // trace is the last step line. The `[read] machine` and `[read] repo`
    // lines print only after the key answers, so their absence here is part
    // of the story: the run stopped exactly at the step that wedged.
    assert!(
        run.text
            .contains("[read] config         : reading the configuration file")
            && run
                .text
                .contains("[read] identity       : resolving this machine's archive partition")
            && run.text.contains("[read] body cache     : ")
            && run
                .text
                .contains("[read] master key     : reading the master key file"),
        "the key step is traced, after every step before it printed its trace: {}",
        run.text
    );
    assert!(
        run.text
            .contains("🔴 reading the master key file did not answer within 2s"),
        "the verdict names the key step and its budget: {}",
        run.text
    );
    assert!(
        run.text
            .contains("the master key was not read, so nothing in the archive could be decrypted"),
        "the consequence is the exact one a wedged key read has: {}",
        run.text
    );
    assert!(
        !run.text.contains("[read] archive"),
        "the archive was never dialed — the key wedge stopped the run before the remote step: {}",
        run.text
    );
}

/// A destination that answers the connection and then says nothing is a
/// timeout; a destination that refuses the connection is a measured error,
/// and the two must never wear each other's wording. This is the control
/// case: `127.0.0.1:1` refuses instantly, so `read` exits 3 through the
/// existing remote-error path and never claims a deadline expired.
#[test]
fn a_refused_connection_is_an_error_not_a_timeout() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let key = sandbox.path().join("key.json");
    plant_key_file(&key);
    plant_destinated_config(sandbox.path(), "http://127.0.0.1:1", &key, true)
        .expect("write the destination config");
    let started = Instant::now();
    let run = run_bounded(read_command(
        sandbox.path(),
        &[
            "read",
            "--destination",
            "blackhole",
            "--session",
            "w1052.synthetic-session",
        ],
        None,
    ));
    assert_eq!(
        run.code,
        Some(3),
        "a refused destination is a read that did not finish: {}",
        run.text
    );
    assert!(
        !run.text.contains("did not answer within"),
        "a refusal is measured, not a deadline: {}",
        run.text
    );
    assert!(
        !run.text.contains("INCOMPLETE exit_code=3"),
        "a measured refusal keeps the ordinary error wording, not the timeout's: {}",
        run.text
    );
}

/// The paved path: a healthy local read answers quickly, exits 0, prints the
/// report it exists for, and never prints the timeout's wording — the
/// deadline guards the wedged case, never the answered one. The traces are
/// all present, in the family the observed incident's five lines set.
#[test]
fn a_healthy_read_stays_green_and_traceable() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let repo = sandbox.path().join("repo");
    let key = sandbox.path().join("key.json");
    // A real local archive with one synthetic session, through the product's
    // own sealed-shard + push path — the read below must succeed against it.
    let stage = sandbox.path().join("stage");
    let cfg = chat_stasher::store::StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.clone(),
        connections: 1,
        options: Default::default(),
        cache_dir: Some(sandbox.path().join("rustic-cache")),
        no_cache: false,
    };
    let mk = rustic_core::repofile::MasterKey::new();
    chat_stasher::store::write_sealed_shard(
        chat_stasher::store::StageWriter::Collect,
        &stage,
        "w1052-fixture",
        "w1052.synthetic-session",
        &[r#"{"synthetic":true}"#.to_string()],
    )
    .expect("write the synthetic shard");
    chat_stasher::store::persist_key_file(&cfg, &mk).expect("persist the synthetic key");
    chat_stasher::store::BackupStore::new(cfg, "w1052-fixture".to_string())
        .push(&stage, &mk)
        .expect("push the synthetic fixture");
    plant_destinated_config(sandbox.path(), "http://127.0.0.1:1", &key, false)
        .expect("write the machine config");
    let started = Instant::now();
    let run = run_bounded(read_command(
        sandbox.path(),
        &[
            "read",
            "--repo",
            repo.to_str().expect("utf-8 sandbox path"),
            "--key-file",
            key.to_str().expect("utf-8 key path"),
            "--machine",
            "w1052-fixture",
            "--session",
            "w1052.synthetic-session",
            "--keep-ssh-masters",
        ],
        None,
    ));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "a healthy local read needs none of its budgets (1800 s for the archive step alone); \
         took {:?}",
        started.elapsed()
    );
    assert_eq!(
        run.code,
        Some(0),
        "a healthy read keeps its success: {}",
        run.text
    );
    assert!(
        run.text.contains("[read] archive        : reading session"),
        "the archive trace shows the step a healthy run takes too: {}",
        run.text
    );
    assert!(
        run.text.contains("[read] concat len"),
        "the report line reads what this run was for: {}",
        run.text
    );
    assert!(
        !run.text.contains("did not answer within") && !run.text.contains("INCOMPLETE exit_code=3"),
        "an answered run prints no timeout wording: {}",
        run.text
    );
}
