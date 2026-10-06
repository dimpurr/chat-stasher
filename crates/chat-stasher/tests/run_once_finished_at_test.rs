//! W830 — `run-state.json`'s `finished_at_unix` must be the second the pass
//! **ended**, because that is what the field promises: "Wall-clock seconds
//! since the epoch at the moment the pass ended" (`src/runstate.rs`).
//!
//! The bug, exactly. `RunState::new` stamps `now_unix()` while the pass is
//! still running — `run_once_pass` builds the pessimistic record before
//! `collect`, and `cmd_run_once` only overwrites `duration_ms` before
//! `runstate::save`. So the record claims the pass finished when it *began*,
//! by exactly the pass duration: a `duration_ms` of 3022 sits next to a
//! `finished_at_unix` taken before the work it measures. Every reader of that
//! field is therefore wrong by the whole pass: `summarize` calls a run
//! staler than it is, `last_push_state` and `archived_through` date a push
//! and a watermark that had not happened yet, and `waiting_to_upload` counts
//! shards as waiting from a moment before they were archived.
//!
//! Why the rig stalls the pass. At second granularity a healthy pass is far
//! too short to tell the two stamps apart — measured on this machine: 0.13 s
//! wall / 22-30 ms recorded for a no-op, 1.5 s for a first pass — so a test
//! without a stall would pass before the fix by luck of rounding. The rig
//! therefore holds the pass open for three full seconds at a known instant
//! and then reads the record: the fixed code must date the pass at or after
//! the release, the buggy code necessarily dates it at or before the moment
//! the pass was blocked.
//!
//! The stall arm is `#[cfg(unix)]`. It holds the pass open with a POSIX FIFO
//! in the `--key-file` position, which blocks `store::load_key_file` until a
//! writer shows up, and the writer's first successful non-blocking open is an
//! exact signal that the pass is inside that read — no guessing, no
//! timing window. Windows has no FIFO through the standard API (a named pipe
//! needs `CreateNamedPipeW`, and opening one as a client fails rather than
//! waits), so the property cannot be *stalled* there. That is not an excuse
//! to drop it silently: `finished_at_unix_covers_the_duration_it_records` runs
//! on every platform and pins the relation the field doc promises from the side
//! Windows can express, and this arm is documented rather than faked — the
//! same shape `b70_inboxfsync_test.rs` uses for its `chmod` rig.

use std::fs;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
#[cfg(unix)]
use std::time::Duration;
use std::time::SystemTime;

#[path = "../src/test_support.rs"]
mod test_support;

/// How long the pass is held open. Chosen to be safely above any plausible
/// process start-up, so "the record predates the release by three seconds"
/// cannot be confused with a slow spawn on either side of the assertion.
#[cfg(unix)]
const STALL_SECS: u64 = 3;

const MACHINE: &str = "mbp-test";
const SESSION: &str = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

/// Bytes handed to the blocked reader. The pass only has to *read* them, and
/// whether the masterkey then parses is irrelevant to the timestamps: what is
/// under test is when the record was written, not how the pass ended.
#[cfg(unix)]
const KEY_PAYLOAD: &[u8] = br#"{"version":"v0","scrypt":{"N":16384,"r":8,"p":1,"salt":"00"}}"#;

/// The real binary with every ambient path redirected into `sandbox`.
fn command(sandbox: &Path, args: &[&str]) -> Command {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W830 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    cmd.args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry);
    cmd
}

/// Run the binary to completion.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    command(sandbox, args).output().unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// Write one sealed shard for `SESSION` under `MACHINE`.
fn write_shard(stage: &Path) {
    let dir = stage
        .join("sessions")
        .join(MACHINE)
        .join(SESSION)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("000001.jsonl"),
        cc_line("2025-01-15T12:34:56.789Z") + "\n" + &cc_line("2025-01-15T13:45:07Z") + "\n",
    )
    .unwrap();
}

/// A stage that already holds a sealed shard, plus a config that forces the
/// pass to reach the push path even though nothing new was collected. Without
/// `push_only_if_changed = false` a pass with no new debt can skip the step
/// this whole rig parks in.
fn seed_stage(sandbox: &Path) -> PathBuf {
    let stage = sandbox.join("stage");
    write_shard(&stage);

    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "push_only_if_changed = false\n",
    )
    .unwrap();
    stage
}

/// Whole seconds since the epoch, the unit `finished_at_unix` is recorded in.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(unix)]
fn secs_of(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// `run-state.json` as written by the pass that just ran.
fn recorded_state(sandbox: &Path) -> serde_json::Value {
    let path = sandbox
        .join("data")
        .join("chat-stasher")
        .join("state")
        .join("run-state.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "run-state.json must exist after a pass ({e}): {}",
            path.display()
        )
    });
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("run-state.json must parse: {e}\n{raw}"))
}

/// The key file *is* a FIFO: an ordinary `File::open` for reading waits until
/// a writer arrives, which is the whole stall.
#[cfg(unix)]
fn make_fifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo({}) must succeed", path.display());
}

/// Hold the pass open in `store::load_key_file` and report the instant it
/// arrived, which is the instant the record's own clock must not be read at.
///
/// The rendezvous is exact: a non-blocking write-open of a FIFO with no
/// reader fails with `ENXIO`, so the first one that succeeds means the pass
/// is now inside its blocking read. Only then does the clock start, and only
/// then does the three seconds of stall apply.
#[cfg(unix)]
fn block_reader(fifo: &Path) -> SystemTime {
    use std::os::unix::fs::OpenOptionsExt;

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(mut writer) => {
                let blocked_at = SystemTime::now();
                std::thread::sleep(Duration::from_secs(STALL_SECS));
                if let Err(e) = writer.write_all(KEY_PAYLOAD) {
                    panic!("releasing the blocked reader must work: {e}");
                }
                // Dropping the writer is the EOF that lets the read finish.
                return blocked_at;
            }
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the pass never opened the key file: {e}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Keep serving the FIFO after the first release, in case the pass reads the
/// key file a second time. Detached on purpose: it only exists so a second
/// read cannot hang the pass past its own assertions, and the test process
/// ends it.
#[cfg(unix)]
fn keep_serving(fifo: PathBuf) {
    use std::os::unix::fs::OpenOptionsExt;

    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if let Ok(mut writer) = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                drop(writer.write_all(KEY_PAYLOAD));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

/// The regression itself: a pass that is held open for three seconds must be
/// recorded as having finished *after* it was released, not when it started.
///
/// Shown failing against the unfixed code: there `finished_at_unix` is stamped
/// by `RunState::new` before the pass reaches the key file, so it equals the
/// second the reader was blocked, and `finished >= blocked + 3` is false by
/// construction — independent of how fast the machine is.
#[test]
#[cfg(unix)]
fn finished_at_unix_is_the_second_the_pass_ended() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = seed_stage(sandbox);
    let key = sandbox.join("keys").join("masterkey.json");
    fs::create_dir_all(key.parent().unwrap()).unwrap();
    make_fifo(&key);

    let fifo = key.clone();
    let blocker = std::thread::spawn(move || block_reader(&fifo));

    let spawn_second = now_secs();
    let child = command(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--repo",
            sandbox.join("repo").to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    )
    .spawn()
    .unwrap();

    let blocked = blocker.join().expect("key-file blocker thread");
    // Polling starts before the child is waited for, so a second read of the
    // key file finds a writer instead of hanging the pass.
    keep_serving(key);
    let output = child.wait_with_output().unwrap();

    let state = recorded_state(sandbox);
    let finished = state["finished_at_unix"]
        .as_u64()
        .unwrap_or_else(|| panic!("finished_at_unix must be a number: {state}"));
    let duration_ms = state["duration_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("duration_ms must be a number: {state}"));
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Premise: the pass really was parked in the key-file read for the whole
    // stall. A record that claims a sub-second pass means the rig never
    // engaged, and the timestamps below would then prove nothing.
    assert!(
        duration_ms >= STALL_SECS * 1000,
        "the pass must be held open for at least {STALL_SECS}s, got duration_ms={duration_ms}\n\
         stdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The record must date the pass at or after the release. Before the fix
    // this is the assertion that fails: the stamp was taken while the pass was
    // still blocked, so it is at most the second the block began.
    let blocked_second = secs_of(blocked);
    assert!(
        finished >= blocked_second + STALL_SECS,
        "finished_at_unix={finished} must be at least {STALL_SECS}s after the pass was blocked \
         at {blocked_second} — a record written when the pass ends, not when it starts"
    );

    // The same promise read from the other end: the recorded duration has to
    // fit between the pass start and the recorded finish.
    assert!(
        finished >= spawn_second + duration_ms / 1000,
        "finished_at_unix={finished} must cover spawn={spawn_second} plus the recorded \
         duration_ms={duration_ms} — the finish cannot predate the work it measures"
    );

    // And it cannot be in the future either: this is a wall-clock stamp of a
    // pass that has already returned.
    assert!(
        finished <= now_secs(),
        "finished_at_unix={finished} must not be in the future"
    );
}

/// The relation the field doc promises, on every platform.
///
/// This arm cannot tell the two stamps apart — a pass this short lands in the
/// same second either way — so it is not the regression test; it pins the
/// invariant (`finished` covers `spawn + duration`) where the stalled arm
/// cannot run, which is the side Windows can express. The arm that can
/// separate start from end is `finished_at_unix_is_the_second_the_pass_ended`.
#[test]
fn finished_at_unix_covers_the_duration_it_records() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = seed_stage(sandbox);

    let spawn_second = now_secs();
    let output = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--repo",
            sandbox.join("repo").to_str().unwrap(),
            "--key-file",
            sandbox
                .join("keys")
                .join("masterkey.json")
                .to_str()
                .unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(
        output.status.success(),
        "an unstalled pass must exit 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let state = recorded_state(sandbox);
    let finished = state["finished_at_unix"]
        .as_u64()
        .unwrap_or_else(|| panic!("finished_at_unix must be a number: {state}"));
    let duration_ms = state["duration_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("duration_ms must be a number: {state}"));

    assert!(
        finished >= spawn_second + duration_ms / 1000,
        "finished_at_unix={finished} must cover spawn={spawn_second} plus duration_ms={duration_ms}"
    );
    assert!(
        finished <= now_secs(),
        "finished_at_unix={finished} must not be in the future"
    );
}
