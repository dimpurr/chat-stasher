//! W242 item 1 — two machines pushing into one destination **at the same time**.
//!
//! This is ADR-016 Decision 4's "measure before designing a lock" and
//! ADR-016's own second open item, which records two earlier attempts that both
//! degenerated into an asymmetric run (one machine wrote, the other reported
//! `files_new=0 · data_added=0`) because the second machine's content was
//! already in the repository. A test that cannot make both clients write is not
//! a test of concurrency, so these cases deliberately give both clients
//! genuinely new content — including one case where the two machines carry
//! byte-identical new shards, the construction ADR-016 says is required to
//! reach the narrow same-object race at all.
//!
//! What the repository can and cannot collide on was measured, not assumed.
//! Every stored object `push` writes is content-addressed over bytes that
//! contain a fresh random 128-bit AEAD nonce
//! (`rustic_core-0.12.0/src/crypto/aespoly1305.rs:119-124`), so two clients can
//! never compute the same id for a data pack, an index or a snapshot: pushing
//! one identical stage into two fresh repositories with one masterkey shares
//! **zero** object ids. The one exception is the repository `config`, whose
//! local path is fixed — the temp name is a pure function of the file type
//! (`rustic_backend-0.6.2/src/local.rs:93-98` names it `config`, and `:546-547`
//! builds the temp name as `filename + "-tmp-"`, with no pid, host or nonce).
//! Two clients therefore race on exactly one thing: initialising the same
//! *fresh* destination at the same instant. The opendal backend is measured
//! beside it as the second data point: its temp name carries a random suffix
//! (`opendal-core-0.57.0/src/raw/path.rs:222-225`), and it does not collide.
//!
//! That race is real and reproducible, and its outcome is safe in all three
//! senses this repository cares about:
//!
//! * **loud** — the loser exits non-zero and names the `config` move it lost;
//!   it never exits 0 having archived nothing;
//! * **non-destructive** — the winner's snapshot reads back byte-perfect, the
//!   loser's stage is untouched, and no object is left half-written under its
//!   final name;
//! * **self-healing** — re-running the loser's push succeeds and archives its
//!   data byte-perfect.
//!
//! # "At the same time" is a property of the harness, and it was measured
//!
//! The first version of the process-level cases here waited for the first client
//! before spawning the second (`wait(a.spawn_push(repo))`, then
//! `wait(b.spawn_push(repo))`). That is strictly **sequential**: the second
//! child does not exist until the first has exited. Measured, that harness
//! produced 0 collisions in 6 rounds, so the loser branch was unreachable at
//! process level and the collision rate a reader would have attributed to the
//! repository was produced by the harness instead.
//!
//! The cases below spawn both clients before waiting for either, and where the
//! platform has one they are released from a **start gate** ([`GATE`]): both
//! children block on a line from a pipe *before* exec'ing the CLI, so neither
//! can reach `open_or_init` before the other process exists and is running.
//!
//! Measured on 2026-09-29 on one machine, one destination per round, two
//! clients with separate config, cache and key, small (few-KB) stages:
//!
//! | harness | destination | rounds | rounds with a `config` collision |
//! |---|---|---|---|
//! | sequential — `wait(a)`, then spawn and wait `b` | fresh | 6 | **0** |
//! | both spawned, then both waited, no gate | fresh | 12 | **12** |
//! | two parent threads released by a `Barrier`, one spawn each | fresh | 12 | **12** |
//! | both released from the pipe gate | fresh | 12 | **12** |
//! | `opendal:fs` backend, released from the pipe gate | fresh | 12 | **0** |
//! | pipe gate | already created by a third machine | 12 | **0**, both land |
//! | pipe gate, byte-identical content | already created by a third machine | 12 | **0**, both land |
//!
//! So the collision is not coincidental: with genuinely concurrent clients it
//! happened on **every** round measured, and on every one of those rounds all
//! three properties above held. An earlier note in this file claimed a
//! `Command::spawn` pair is "spaced wider than the collision window — 0
//! collisions in 18 rounds": that is not reproducible for concurrent spawns,
//! and the 0 belongs to the sequential pattern.
//!
//! Two ways to lose the concurrency again, both of which this file had to be
//! caught doing: waiting for the first client before spawning the second (the
//! original), and `push_a.release().wait()` on one line followed by the same
//! for `b` (releasing `b` only once `a` has exited). Both produce 0 collisions
//! against a repository that collides 12 times out of 12, which is why the
//! sequential row above is in the table at all: a harness that stops being
//! concurrent does not fail, it goes quiet.
//!
//! Because the gate makes the outcome deterministic here, the twelve-round
//! reproduction asserts that **at least one** round collides (see
//! `the_gated_pair_collides_on_a_fresh_destination`), so that going quiet is a
//! failure rather than a green run. It is `#[ignore]`d because a CI machine
//! whose scheduler spreads the two children must not turn a property of this
//! harness into a red gate; the small CI variant
//! (`two_clients_first_pushing_a_fresh_destination_never_corrupt_it`) asserts
//! only the envelope, which holds whether or not a client loses, and reports the
//! count it saw.
//!
//! Both levels assert the same three properties for the loser: a loud
//! non-zero/`Err` outcome that names the lost object, an untouched stage, and a
//! retry that archives its bytes exactly.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};

/// The start gate, as `(program, script)`, where the platform has one.
///
/// The script blocks on a line from stdin *before* exec'ing the CLI, and passes
/// the binary and its arguments as positional parameters, so no argument is
/// re-parsed by the shell.
///
/// Windows has no gate: `cmd` is not `sh`, `CommandExt::pre_exec` is unix-only,
/// and there is no portable way to block a child between fork and exec. The
/// process-level cases still run there and still assert the same envelope — the
/// two clients are genuinely concurrent, spawned before either is waited for —
/// but they are spaced by however long `spawn` takes, so the collision *rate*
/// is that platform's scheduler's business. It is reported there, never
/// asserted: a test that fails because one machine's scheduler spread two
/// processes is not testing the repository.
#[cfg(unix)]
const GATE: Option<(&str, &str)> = Some(("sh", r#"read _; exec "$0" "$@""#));
#[cfg(not(unix))]
const GATE: Option<(&str, &str)> = None;

/// Whether the platform's harness can put both children at `open_or_init`
/// together, as opposed to merely running them concurrently.
fn gate_available() -> bool {
    GATE.is_some()
}

/// Which backend the two clients write to.
///
/// `opendal:fs` is the second data point for the collision question and runs
/// through the same opendal code path production uses for `opendal:sftp`. It
/// needs its root as a backend option, not in the repository string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Local,
    OpendalFs,
}

impl Backend {
    /// Arguments naming `repo` to a `push` or a `read`.
    fn args(self, repo: &Path) -> Vec<String> {
        match self {
            Backend::Local => vec!["--repo".to_string(), repo.display().to_string()],
            Backend::OpendalFs => vec![
                "--repo".to_string(),
                "opendal:fs".to_string(),
                "--option".to_string(),
                format!("root={}", repo.display()),
            ],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::OpendalFs => "opendal:fs",
        }
    }
}

/// The no-harness registry every test runs against, so `push`'s stage check
/// never walks the real machine's harness directories.
fn write_registry(sandbox: &Path) -> PathBuf {
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(sandbox).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W242 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    registry
}

/// One simulated machine: its own home, config, data root, cache and key.
///
/// The metadata cache is named in the client's **own config**
/// (`rustic_cache_dir`), not left to `dirs::cache_dir()`, because that
/// resolution is not portable: macOS reads `$HOME/Library/Caches`, Linux
/// `$XDG_CACHE_HOME`, and Windows `%LOCALAPPDATA%` from the known-folder API,
/// **ignoring every environment variable** (`dirs-6.0.0` `src/win.rs:10` →
/// `known_folder_local_app_data`). Two simulated machines sharing a real cache
/// would be sharing ADR-016's third open item — the cache-directory race —
/// with the collision under test, and on Windows the per-client `HOME` below
/// would not have separated them at all. `XDG_CACHE_HOME` is set as well, but
/// only the config key holds everywhere.
struct Client {
    machine: String,
    dir: PathBuf,
    registry: PathBuf,
    key: PathBuf,
    stage: PathBuf,
    backend: Backend,
}

fn client_at(sandbox: &Path, machine: &str, key: PathBuf, backend: Backend) -> Client {
    let dir = sandbox.join(format!("{machine}-{}", backend.label().replace(':', "-")));
    for sub in ["home", "config", "data", "state", "cache"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    // The key path is chosen by the caller and may point into another
    // machine's tree (the shared-masterkey case), so its directory is created
    // here rather than assumed to exist.
    fs::create_dir_all(key.parent().unwrap()).unwrap();
    let config = dir.join("config").join("chat-stasher");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!("rustic_cache_dir = \"{}\"\n", dir.join("cache").display()),
    )
    .unwrap();
    Client {
        machine: machine.to_string(),
        key,
        stage: dir.join("stage"),
        registry: write_registry(sandbox),
        dir,
        backend,
    }
}

/// A machine with a masterkey file of its own.
fn client(sandbox: &Path, machine: &str, backend: Backend) -> Client {
    let key = sandbox.join(machine).join("key.json");
    client_at(sandbox, machine, key, backend)
}

/// A machine opening the destination with a masterkey file that already exists
/// — one user, several machines, one key.
fn client_sharing_key(sandbox: &Path, machine: &str, key: &Path, backend: Backend) -> Client {
    client_at(sandbox, machine, key.to_path_buf(), backend)
}

impl Client {
    /// The CLI with this machine's environment, or any other program with it.
    fn command_of(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", self.dir.join("home"))
            .env("USERPROFILE", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("CHAT_STASHER_REGISTRY", &self.registry);
        cmd
    }

    fn push_args(&self, repo: &Path) -> Vec<String> {
        let mut args = vec![
            "push".to_string(),
            "--stage".to_string(),
            self.stage.display().to_string(),
            "--machine".to_string(),
            self.machine.clone(),
            "--key-file".to_string(),
            self.key.display().to_string(),
            "--keep-ssh-masters".to_string(),
        ];
        args.extend(self.backend.args(repo));
        args
    }

    fn read_args(&self, repo: &Path) -> Vec<String> {
        let mut args = vec![
            "read".to_string(),
            "--all-machines".to_string(),
            "--full-ids".to_string(),
            "--key-file".to_string(),
            self.key.display().to_string(),
            "--keep-ssh-masters".to_string(),
        ];
        args.extend(self.backend.args(repo));
        args
    }

    fn run(&self, args: &[String]) -> Output {
        self.command_of(env!("CARGO_BIN_EXE_chat-stasher"))
            .args(args)
            .output()
            .unwrap()
    }

    /// Spawn `push` without waiting, through the start gate where there is one,
    /// so two clients can be released together and then waited for.
    fn spawn_push(&self, repo: &Path) -> Push {
        let args = self.push_args(repo);
        let mut cmd = match GATE {
            Some((shell, script)) => {
                let mut cmd = self.command_of(shell);
                cmd.arg("-c")
                    .arg(script)
                    .arg(env!("CARGO_BIN_EXE_chat-stasher"));
                cmd.stdin(Stdio::piped());
                cmd
            }
            None => {
                let mut cmd = self.command_of(env!("CARGO_BIN_EXE_chat-stasher"));
                cmd.stdin(Stdio::null());
                cmd
            }
        };
        let child = cmd
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = child;
        let gate = child.stdin.take();
        Push { child, gate }
    }

    fn push(&self, repo: &Path) -> Output {
        self.run(&self.push_args(repo))
    }

    /// Every session's archived bytes, as a map from full session id to sha256.
    fn read_archive(&self, repo: &Path) -> BTreeMap<String, String> {
        let out = self.run(&self.read_args(repo));
        assert!(
            out.status.success(),
            "read --all-machines failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        parse_session_shas(&String::from_utf8_lossy(&out.stdout))
    }

    /// Write one sealed shard, as raw bytes, so a test can hand two machines
    /// byte-identical content. Returns the path it wrote.
    fn write_shard(&self, session: &str, body: &str) -> PathBuf {
        let path = self.shard_path(session);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn shard_path(&self, session: &str) -> PathBuf {
        self.stage
            .join("sessions")
            .join(&self.machine)
            .join(session)
            .join("000")
            .join("000001.jsonl")
    }
}

/// A `push` that has been spawned and has not yet run the CLI's own code.
///
/// With a gate the child is a `sh` blocked on a line from its stdin; without
/// one it is the CLI itself, already running. Either way the two clients of a
/// case exist before either is waited for, which is what makes the case
/// concurrent rather than sequential.
struct Push {
    child: Child,
    gate: Option<ChildStdin>,
}

impl Push {
    /// Let the child past the gate. Dropping the pipe afterwards closes it, so
    /// the child proceeds whether the write landed or not — a failed write
    /// releases it by EOF, which is why the result is deliberately ignored.
    fn release(mut self) -> Self {
        if let Some(mut stdin) = self.gate.take() {
            stdin.write_all(b"\n").ok();
        }
        self
    }

    fn wait(self) -> Output {
        self.child.wait_with_output().unwrap()
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A claude-code line carrying `ts`, with `pad` bytes of filler behind it.
fn cc_line(session: &str, ts: &str, pad: usize) -> String {
    let filler: String = (0..pad).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    format!(
        r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{filler}"}},"uuid":"u1","timestamp":"{ts}"}}"#
    ) + "\n"
}

/// Hand `b` the masterkey `a` just created, by copying the key file, and remove
/// the throwaway repository — so both clients then see the same *absent*
/// destination, the real "two machines configured at once" shape.
fn share_masterkey(a: &Client, b: &Client, sandbox: &Path) {
    let scratch = sandbox.join(format!(
        "scratch-repo-{}",
        a.backend.label().replace(':', "-")
    ));
    let out = a.push(&scratch);
    assert!(
        out.status.success(),
        "seeding the shared masterkey failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(a.key.exists(), "the seeding push wrote no key file");
    fs::copy(&a.key, &b.key).unwrap();
    fs::remove_dir_all(&scratch).unwrap();
}

/// Every file below `root` whose name carries rustic's temp marker.
fn leftover_temp_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("-tmp-"))
            {
                found.push(path);
            }
        }
    }
    found
}

/// One round of the fresh-destination race: two clients, both released from the
/// gate (where there is one), both waited for, then the envelope checked.
///
/// Returns the number of clients that lost the `config` move, so a caller can
/// report the rate without the envelope assertions living in two places.
///
/// The envelope holds whatever the scheduler does — including on a round where
/// nobody loses — which is why the CI case can run this and assert nothing
/// about the count:
///
/// * a failure must name the `config` object (a silent success having archived
///   nothing would be the invariant-1 failure);
/// * at least one client must win (both losing would mean the destination was
///   never created);
/// * no object may be left visible under a temp name;
/// * every winner's bytes read back exactly as staged;
/// * the loser's stage is untouched and its retry archives its bytes exactly.
fn fresh_destination_round(sandbox: &Path, backend: Backend, round: usize) -> usize {
    // One root per round, so a round's stage holds only its own shards and no
    // round inherits the previous one's config, cache or repository.
    let root = sandbox.join(format!(
        "{}-round{round}",
        backend.label().replace(':', "-")
    ));
    fs::create_dir_all(&root).unwrap();
    let repo = root.join("repo");
    let a = client(&root, "mbp-a", backend);
    let b = client(&root, "mbp-b", backend);
    let session_a = format!("claude-code.mbp-a.round{round}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
    let session_b = format!("claude-code.mbp-b.round{round}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
    let body_a = cc_line(&session_a, "2025-03-01T10:00:00Z", 4096);
    let body_b = cc_line(&session_b, "2025-03-02T10:00:00Z", 4096);
    a.write_shard(&session_a, &body_a);
    b.write_shard(&session_b, &body_b);
    share_masterkey(&a, &b, &root);

    // Both clients exist and are held at the gate before either is released, so
    // neither can see the destination created by the other before it looks.
    let push_a = a.spawn_push(&repo);
    let push_b = b.spawn_push(&repo);
    // Both released before either is waited for. `push.release().wait()` on one
    // line each would be sequential again, which is the very mistake this file
    // was rewritten to stop making.
    let (push_a, push_b) = (push_a.release(), push_b.release());
    let out_a = push_a.wait();
    let out_b = push_b.wait();

    let mut winners: Vec<(&Client, &str, &str)> = Vec::new();
    let mut losers: Vec<(&Client, &str, &str)> = Vec::new();
    for (machine, out, session, body) in [
        (&a, &out_a, session_a.as_str(), body_a.as_str()),
        (&b, &out_b, session_b.as_str(), body_b.as_str()),
    ] {
        if out.status.success() {
            winners.push((machine, session, body));
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("config"),
                "round {round}: a failed client must name the config object it lost, \
                 but said:\n{stderr}"
            );
            losers.push((machine, session, body));
        }
    }
    assert!(
        !winners.is_empty(),
        "round {round}: both clients failed, so the destination was never created"
    );

    // No object is ever visible under its final name half-written.
    let leftovers = leftover_temp_files(&repo);
    assert!(
        leftovers.is_empty(),
        "round {round}: leftover temp objects: {leftovers:?}"
    );

    // Whatever was archived reads back byte-perfect.
    let archived = a.read_archive(&repo);
    for (machine, session, body) in &winners {
        assert_eq!(
            archived.get(*session).map(String::as_str),
            Some(sha256_hex(body.as_bytes()).as_str()),
            "round {round}: {} archived {session} with the wrong bytes",
            machine.machine
        );
    }

    // The loser can recover, from a stage it never touched.
    let lost = losers.len();
    for (machine, session, body) in losers {
        assert_eq!(
            fs::read(machine.shard_path(session)).unwrap(),
            body.as_bytes(),
            "round {round}: the failed push disturbed the stage"
        );
        let retry = machine.push(&repo);
        assert!(
            retry.status.success(),
            "round {round}: {} could not recover on a retry: {}",
            machine.machine,
            String::from_utf8_lossy(&retry.stderr)
        );
        let archived = machine.read_archive(&repo);
        assert_eq!(
            archived.get(session).map(String::as_str),
            Some(sha256_hex(body.as_bytes()).as_str()),
            "round {round}: {} recovered with the wrong bytes",
            machine.machine
        );
    }
    lost
}

/// Two clients that both first-push the same *fresh* destination at once.
///
/// This is the only reachable collision in the whole push path (see the module
/// doc): the repository `config` has a fixed path and a fixed temp name. It is
/// run against the local backend and against `opendal:fs`, whose temp name
/// carries a random suffix, in the same three rounds each — the comparison the
/// measurement table in the module doc reports.
///
/// Nothing here asserts *that* a client loses: the envelope is the contract,
/// and it holds on a round where both win. The deterministic reproduction is
/// the `#[ignore]`d case below.
#[test]
fn two_clients_first_pushing_a_fresh_destination_never_corrupt_it() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();

    for backend in [Backend::Local, Backend::OpendalFs] {
        let mut losses = 0;
        for round in 0..3 {
            losses += fresh_destination_round(sandbox, backend, round);
        }
        // Reported, not asserted: which rounds collide is the scheduler's
        // business, and the rate is in the module doc.
        eprintln!(
            "W242: {} backend — {losses} of 3 gated rounds had a client lose the config move",
            backend.label()
        );
    }
}

/// The deterministic reproduction, at process level: the gate holds both
/// clients before either runs, so both reach `open_or_init` in the same instant
/// and the loser branch is exercised on every round measured.
///
/// Twelve rounds, because the claim being pinned is a rate: measured 12 of 12
/// with the gate, and 0 of 6 with the sequential harness this file used to have.
/// The assertion is "at least one", not "twelve", so that a machine whose
/// scheduler still spreads the two children fails loudly on a real regression
/// (the gate stops gating) rather than on a coincidence of timing.
///
/// `#[ignore]`d: on a platform with no gate the count is a scheduler property
/// and asserting it there would be asserting the CI machine, not the
/// repository. Run with
/// `cargo test -p chat-stasher --test w242_multiclient_concurrency_test -- --ignored`.
#[test]
#[ignore = "rate measurement (12 rounds); the CI variant is \
            two_clients_first_pushing_a_fresh_destination_never_corrupt_it"]
fn the_gated_pair_collides_on_a_fresh_destination() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let mut losses = 0;
    for round in 0..12 {
        losses += fresh_destination_round(sandbox, Backend::Local, round);
    }
    eprintln!("W242: init collisions observed in {losses} of 12 gated rounds");
    if gate_available() {
        assert!(
            losses > 0,
            "the start gate must put both clients at open_or_init together — measured \
             12 of 12 rounds — so a round count of 0 means the harness stopped gating, \
             not that the collision disappeared"
        );
    }
}

/// Steady state: the destination already exists and both machines have new,
/// distinct content. This is the case the hourly agent actually runs, and it is
/// where a lock would have to earn its keep if the "danger is in delete, not in
/// write" reading of ADR-016 were wrong.
#[test]
fn two_clients_with_new_content_into_an_existing_destination_both_land() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let repo = sandbox.join("repo");
    let backend = Backend::Local;

    let a = client(sandbox, "mbp-a", backend);
    let b = client(sandbox, "mbp-b", backend);
    let session_a = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string();
    let session_b = "claude-code.mbp-b.019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string();
    let body_a = cc_line(&session_a, "2025-03-01T10:00:00Z", 20_000);
    let body_b = cc_line(&session_b, "2025-03-02T10:00:00Z", 20_000);
    a.write_shard(&session_a, &body_a);
    b.write_shard(&session_b, &body_b);
    share_masterkey(&a, &b, sandbox);

    // A throwaway machine pre-creates the destination with the same masterkey,
    // so neither client may take the init path.
    let seed = client_sharing_key(sandbox, "seed", &a.key, backend);
    let seed_session = "claude-code.seed.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    seed.write_shard(
        seed_session,
        &cc_line(seed_session, "2025-02-01T00:00:00Z", 64),
    );
    let seeded = seed.push(&repo);
    assert!(
        seeded.status.success(),
        "seeding the destination failed: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );

    let push_a = a.spawn_push(&repo);
    let push_b = b.spawn_push(&repo);
    // Both released before either is waited for. `push.release().wait()` on one
    // line each would be sequential again, which is the very mistake this file
    // was rewritten to stop making.
    let (push_a, push_b) = (push_a.release(), push_b.release());
    let out_a = push_a.wait();
    let out_b = push_b.wait();
    assert!(
        out_a.status.success(),
        "machine-a failed against an existing destination: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert!(
        out_b.status.success(),
        "machine-b failed against an existing destination: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    let archived = a.read_archive(&repo);
    for (session, body) in [(&session_a, &body_a), (&session_b, &body_b)] {
        assert_eq!(
            archived.get(session).map(String::as_str),
            Some(sha256_hex(body.as_bytes()).as_str()),
            "{session} was not archived byte-perfect"
        );
    }
    assert!(
        leftover_temp_files(&repo).is_empty(),
        "a concurrent push left a temp object behind"
    );
}

/// The construction ADR-016 says is required to reach the narrow same-object
/// race: two machines producing **byte-identical new content** at the same
/// time, with no parent snapshot to dedup against, so both are forced to write.
///
/// It is unreachable in practice: every stored object's id covers bytes
/// carrying a fresh random AEAD nonce, so the two clients write two different
/// packs rather than fighting over one temp name. The earlier attempts recorded
/// in ADR-016 degenerated for the opposite reason — the second machine's
/// content was already in the repository, so it had nothing to write.
#[test]
fn two_clients_pushing_byte_identical_new_content_both_land_byte_perfect() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let repo = sandbox.join("repo");
    let backend = Backend::Local;

    let a = client(sandbox, "mbp-a", backend);
    let b = client(sandbox, "mbp-b", backend);
    let session_a = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string();
    let session_b = "claude-code.mbp-b.019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string();
    // Byte-identical shard content on both machines.
    let shared = cc_line("shared-session", "2025-04-01T09:00:00Z", 40_000);
    a.write_shard(&session_a, &shared);
    b.write_shard(&session_b, &shared);
    share_masterkey(&a, &b, sandbox);

    let seed = client_sharing_key(sandbox, "seed", &a.key, backend);
    let seed_session = "claude-code.seed.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    seed.write_shard(
        seed_session,
        &cc_line(seed_session, "2025-02-01T00:00:00Z", 64),
    );
    assert!(seed.push(&repo).status.success());

    let push_a = a.spawn_push(&repo);
    let push_b = b.spawn_push(&repo);
    // Both released before either is waited for. `push.release().wait()` on one
    // line each would be sequential again, which is the very mistake this file
    // was rewritten to stop making.
    let (push_a, push_b) = (push_a.release(), push_b.release());
    let out_a = push_a.wait();
    let out_b = push_b.wait();
    for (name, out) in [("machine-a", &out_a), ("machine-b", &out_b)] {
        assert!(
            out.status.success(),
            "{name} failed on byte-identical concurrent content: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("data_added="),
            "{name} printed no summary: {stdout}"
        );
    }

    let archived = a.read_archive(&repo);
    for session in [&session_a, &session_b] {
        assert_eq!(
            archived.get(session).map(String::as_str),
            Some(sha256_hex(shared.as_bytes()).as_str()),
            "{session} was not archived byte-perfect"
        );
    }
    assert!(
        leftover_temp_files(&repo).is_empty(),
        "a concurrent push left a temp object behind"
    );
}

/// The init collision, reproduced **deterministically** rather than by hoping
/// two processes overlap.
///
/// The process-level case above uses the pipe gate, which holds both children
/// before either runs. This one reaches the same code path from inside one
/// process: two threads released from a `Barrier`, each entering
/// `open_or_init` in the same instant. It is the variant that survives when no
/// gate exists for the platform's child processes.
///
/// Each client gets its own `cache_dir`: a shared rustic metadata cache would
/// add ADR-016's third open item, the cache-directory race, on top of the one
/// under test.
///
/// Same envelope as the process-level case — loud, non-destructive, and the
/// loser recovers on a retry over data it never touched.
#[test]
fn two_clients_initialising_one_fresh_repository_at_once_stay_safe() {
    use chat_stasher::store::{BackupStore, StoreConfig};
    use rustic_core::repofile::MasterKey;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn client_cfg(root: &Path, name: &str, repo: &Path, key: &Path) -> StoreConfig {
        StoreConfig {
            repo_root: repo.to_string_lossy().into_owned(),
            key_file: key.to_path_buf(),
            connections: 1,
            options: Default::default(),
            cache_dir: Some(root.join(format!("cache-{name}"))),
            no_cache: false,
        }
    }

    /// Write one shard of known bytes and return (stage root, exact bytes).
    fn stage_with(root: &Path, name: &str, body: &str) -> (PathBuf, Vec<u8>) {
        let stage = root.join(format!("stage-{name}"));
        let session = format!("claude-code.{name}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
        let dir = stage.join("sessions").join(name).join(&session).join("000");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000001.jsonl"), body).unwrap();
        (stage, body.as_bytes().to_vec())
    }

    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let mut races_observed = 0usize;

    for round in 0..12 {
        let root = sandbox.join(format!("round{round}"));
        fs::create_dir_all(&root).unwrap();
        let repo = root.join("repo");
        let key = root.join("masterkey.json");
        let mk = MasterKey::new();
        let (stage_a, body_a) = stage_with(
            &root,
            "mbp-a",
            &cc_line(
                "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
                "2025-05-01T10:00:00Z",
                8192,
            ),
        );
        let (stage_b, body_b) = stage_with(
            &root,
            "mbp-b",
            &cc_line(
                "claude-code.mbp-b.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
                "2025-05-02T10:00:00Z",
                8192,
            ),
        );

        // Spawn both first — a `Barrier` of two cannot release until the second
        // thread arrives, so neither may be joined before both exist.
        let barrier = Arc::new(Barrier::new(2));
        let mut planned: Vec<(String, PathBuf, Vec<u8>, thread::JoinHandle<_>)> =
            [("mbp-a", stage_a, body_a), ("mbp-b", stage_b, body_b)]
                .into_iter()
                .map(|(name, stage, body)| {
                    let cfg = client_cfg(&root, name, &repo, &key);
                    let barrier = barrier.clone();
                    let mk = mk.clone();
                    let machine = name.to_string();
                    let spawned = thread::spawn(move || {
                        barrier.wait();
                        BackupStore::new(cfg, machine.clone()).push(&stage, &mk)
                    });
                    (
                        name.to_string(),
                        root.join(format!("stage-{name}")),
                        body,
                        spawned,
                    )
                })
                .collect();

        let mut winners: Vec<(String, Vec<u8>)> = Vec::new();
        let mut failures: Vec<(String, Vec<u8>, String)> = Vec::new();
        for (name, _stage, body, handle) in planned.drain(..) {
            match handle.join().unwrap() {
                Ok(_) => winners.push((name, body)),
                // `{:#}` prints anyhow's whole context chain; `to_string()` shows only
                // the outermost context ("init new repository") and would hide the
                // object whose move was lost.
                Err(err) => failures.push((name, body, format!("{err:#}"))),
            }
        }
        let observed = winners.len() + failures.len();
        assert_eq!(
            observed, 2,
            "round {round}: a client neither archived nor failed"
        );
        assert!(
            !winners.is_empty(),
            "round {round}: both clients failed, so the destination was never created"
        );
        if !failures.is_empty() {
            races_observed += 1;
        }
        for (name, _body, message) in &failures {
            assert!(
                message.contains("config"),
                "round {round}: {name} failed without naming the config object it lost: {message}"
            );
        }

        // No half-written object is visible under a final name, and whatever
        // landed reads back byte-for-byte.
        assert!(
            leftover_temp_files(&repo).is_empty(),
            "round {round}: the loss left a temp object behind"
        );
        let reader = BackupStore::new(client_cfg(&root, "reader", &repo, &key), "reader".into());
        for (name, body) in &winners {
            let session = format!("claude-code.{name}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
            let (bytes, _) = reader.read_session_concat(name, &session, &mk).unwrap();
            assert_eq!(
                bytes, *body,
                "round {round}: {name} archived different bytes than it staged"
            );
        }

        // The loser recovers on a retry over a stage it never touched.
        for (name, body, _message) in failures {
            let stage = root.join(format!("stage-{name}"));
            let session = format!("claude-code.{name}.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
            let shard = stage
                .join("sessions")
                .join(&name)
                .join(&session)
                .join("000/000001.jsonl");
            assert_eq!(
                fs::read(&shard).unwrap(),
                body,
                "round {round}: the failed push disturbed the stage"
            );
            let cfg = client_cfg(&root, &name, &repo, &key);
            BackupStore::new(cfg, name.clone())
                .push(&stage, &mk)
                .unwrap_or_else(|err| panic!("round {round}: {name} could not recover: {err}"));
            let (bytes, _) = reader.read_session_concat(&name, &session, &mk).unwrap();
            assert_eq!(
                bytes, body,
                "round {round}: {name} recovered with different bytes"
            );
        }
    }

    // Not an assertion about the scheduler: it is reported so a future run that
    // silently stops reproducing the collision is visible in the log.
    eprintln!("W242: init collisions observed in {races_observed} of 12 rounds");
}

/// Guard on the parser the cases above rely on: if `read --all-machines` ever
/// stopped printing a sha256 in the assumed shape, the byte-perfect assertions
/// would silently become vacuous instead of failing.
#[test]
fn archive_reader_reports_one_sha256_per_session() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let repo = sandbox.join("repo");
    let backend = Backend::Local;
    let a = client(sandbox, "mbp-a", backend);
    let b = client(sandbox, "mbp-b", backend);
    let session = "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let body = cc_line(session, "2025-03-01T10:00:00Z", 32);
    a.write_shard(session, &body);
    share_masterkey(&a, &b, sandbox);

    assert!(a.push(&repo).status.success());
    let archived = a.read_archive(&repo);
    assert_eq!(archived.len(), 1, "expected exactly one archived session");
    assert_eq!(
        archived.get(session).map(String::as_str),
        Some(sha256_hex(body.as_bytes()).as_str())
    );
}
