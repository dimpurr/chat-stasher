//! W246 · ADR-034 release criterion ①, measured against a **remote-like**
//! destination: wall-clock time to `read` one ~100 MB session, cold (cache
//! empty, every body block fetched over the link) against warm (cache full,
//! zero body misses and therefore no download), several runs each, median
//! reported.
//!
//! W243 measured the same read against a **local** archive and found the win is
//! small there — warm was 96% of cold, because a local archive has no download
//! to save. That is not the case ADR-034's criterion ① is written about: the
//! criterion is "a machine with a quota set reads the same large session
//! noticeably faster the second time (107 MB: 23 s → < 2 s)", and the 23 s came
//! from W117 measuring a real remote's **download** at ~4.7 MB/s. This file
//! builds that case locally, so the number is measured rather than asserted.
//!
//! This is a manual measurement, not a CI assertion — besides the ~100 MB
//! fixture it starts an ssh server and a throttling proxy. Run it explicitly:
//!
//! ```sh
//! cargo test --release --test w246_remote_cache_test -- --ignored --nocapture
//! ```
//!
//! (release, so the decryption the warm path still pays is the one a user gets).
//!
//! # What is real and what stands in for something
//!
//! | layer | here | why it is the same layer a user has |
//! |---|---|---|
//! | sftp protocol + subsystem | a local `sshd` on a loopback port, throwaway host and client keys in a temp dir | it is OpenSSH `sshd`'s own `internal-sftp`; the client is the `ssh` binary the `openssh` crate drives, exactly as for a real host |
//! | destination backend | `opendal:sftp`, the string production ships | the operator, the `openssh` session and the `ConcurrentLimitLayer` are the ones `BackupStore::backends` builds |
//! | link | a TCP proxy in this file charging bytes to a shared leaky bucket at 4.73 MB/s | only the *rate* is synthetic; the client sees an ordinary, slow TCP connection |
//! | archive | the project's own sealed-shard + `push` path, over that sftp backend | no real conversation, key or destination is touched |
//!
//! **Which layers are skipped: none.** The throttled read travels
//! `chat-stasher → opendal:sftp → ssh → TCP proxy → sshd`, so the thing being
//! saved is the thing that is throttled — bytes on a socket — and not, say,
//! an artificial delay inside the backend.
//!
//! Only the **download** direction is throttled. That is the quantity W117
//! measured and the one criterion ① is about ("107 MB: 23 s" is 107 MB *read*),
//! and it is why the fixture push below is not itself rate-limited into
//! minutes. The upload direction is an ordinary loopback connection.
//!
//! # The assertions are structural, and they are the reason to trust the table
//!
//! Wall-clock numbers are printed, not asserted — they vary by machine. What is
//! asserted is cheap and would catch the failure that matters: a harness that
//! stops throttling does not fail, it goes quiet. So the proxy counts every
//! byte it relays, and the test checks that a **cold** read moved a large
//! fraction of the session across the link (the limiter really is in the path)
//! while a **warm** read moved almost none (the cache really replaced the
//! download).
//!
//! "A large fraction" and not "all of it": the fixture's alphabet soup is not
//! incompressible — see the measurement note in
//! `docs-dev/body-cache-measurement.md` — so the sealed, compressed ciphertext
//! a cold read pulls is about three quarters of the plaintext, and the
//! assertion is calibrated to that rather than to a byte count that was never
//! going to appear on the wire.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MACHINE: &str = "m-alpha";
/// Plaintext bytes of the measured session, spread over this many shards.
const SHARD_BYTES: usize = 2_048_000;
const SHARD_COUNT: u32 = 50; // 102_400_000 B ≈ 102 MB ≈ W117's "107 MB" session
const SAMPLES: u32 = 5;

/// The link this measurement models.
///
/// W117 measured the real remote at "107 MB re-downloaded in 22.6 s", i.e.
/// `107e6 / 22.6 ≈ 4.73 MB/s`, and ADR-034's criterion ① is written against
/// that same pair of numbers. Decimal MB, because that is how W117 wrote it.
const DOWNLOAD_BYTES_PER_SEC: f64 = 4.73e6;

/// Where OpenSSH's server lives. The sftp backend is unix-only
/// (`opendal-service-sftp-0.57.0/src/backend.rs:42`), so a platform without
/// this binary is a platform this measurement does not exist on — the test says
/// so instead of passing quietly.
const SSHD: &str = "/usr/sbin/sshd";

// ---------------------------------------------------------------------------
// The link: a shared rate limiter and the TCP proxy that applies it
// ---------------------------------------------------------------------------

/// A rate limiter shared by every connection the proxy carries.
///
/// The model is the **link**, not the socket: a 4.7 MB/s link does not get
/// faster because the client opened a second connection, so one schedule is
/// shared rather than a per-connection cap that multiplies.
///
/// The scheme books each charge onto a monotone timeline and hands the caller
/// its own waiting time, which it sleeps off outside the lock. There is no
/// banked credit: an idle link does not accumulate a burst allowance to spend
/// later, so the first cold read pays the download time instead of sharing a
/// hoard with the runs after it. The only slack is the granularity of one
/// relay buffer, which is the same slack a real link has.
struct Limiter {
    bytes_per_sec: f64,
    /// Total bytes charged, for the "was the limiter actually in the path"
    /// assertions. Monotone, never reset; callers snapshot it around a run.
    charged: AtomicU64,
    /// The earliest instant the next byte may go (the timeline itself).
    next_free: Mutex<Instant>,
}

impl Limiter {
    fn new(bytes_per_sec: f64) -> Self {
        Self {
            bytes_per_sec,
            charged: AtomicU64::new(0),
            next_free: Mutex::new(Instant::now()),
        }
    }

    /// Book `n` bytes and return how long the caller must wait before sending.
    fn charge(&self, n: usize) -> Duration {
        let mut next_free = self.next_free.lock().expect("limiter lock");
        let now = Instant::now();
        let first = if *next_free > now { *next_free } else { now };
        let due = first + Duration::from_secs_f64(n as f64 / self.bytes_per_sec);
        *next_free = due;
        self.charged.fetch_add(n as u64, Ordering::SeqCst);
        due.saturating_duration_since(now)
    }

    fn charged(&self) -> u64 {
        self.charged.load(Ordering::SeqCst)
    }
}

/// Copy `from` into `to` until the source ends, charging bytes to `limiter`.
///
/// `None` means an unthrottled direction (the upload half of the link).
fn relay(mut from: TcpStream, mut to: TcpStream, limiter: Option<Arc<Limiter>>) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // A reset is how a peer that has gone away looks from here; there
            // is nobody left to tell, and the other direction of this relay
            // decides when the connection is finished.
            Err(_) => break,
        };
        if let Some(limiter) = &limiter {
            let wait = limiter.charge(n);
            if !wait.is_zero() {
                thread::sleep(wait);
            }
        }
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    // The peer may already be gone; a failed half-close has nothing to report.
    drop(to.shutdown(Shutdown::Write));
}

/// A loopback TCP proxy in front of the ssh server, throttling downloads.
///
/// It also counts the connections it carries, because that is the only way to
/// see from outside how much of a read is the sftp path's own session setup:
/// every `opendal` sftp session starts its own ssh `ControlMaster`
/// (`crates/chat-stasher/src/reap.rs:3`), and a read that opens several is
/// paying for them whether or not it downloads anything.
struct ThrottledProxy {
    port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    connections: Arc<AtomicU64>,
}

impl ThrottledProxy {
    fn start(upstream: u16, limiter: Arc<Limiter>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind proxy port");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("proxy local_addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicU64::new(0));
        let count_connections = connections.clone();
        let stop_accepting = stop.clone();
        let accept = thread::spawn(move || {
            while !stop_accepting.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        count_connections.fetch_add(1, Ordering::SeqCst);
                        // BSD (and so macOS) hands an accepted socket the
                        // listener's own `O_NONBLOCK`, which Linux does not.
                        // The relay below is a plain blocking copy, so it is
                        // put back: otherwise the first read returns `EAGAIN`
                        // and the connection is torn down mid-handshake.
                        client
                            .set_nonblocking(false)
                            .expect("blocking relay socket");
                        let limiter = limiter.clone();
                        // Each connection gets its own pair of relay threads and
                        // is then detached: an ssh ControlMaster may hold the
                        // socket open past the read that opened it.
                        thread::spawn(move || {
                            let server = match TcpStream::connect(("127.0.0.1", upstream)) {
                                Ok(server) => server,
                                // The ssh client sees this as a broken pipe and
                                // reports it on its own stderr; this line is
                                // what says the *proxy* is the one that missed.
                                Err(e) => {
                                    eprintln!("[w246] proxy cannot reach sshd: {e}");
                                    return;
                                }
                            };
                            let client_down = client.try_clone().expect("clone client socket");
                            let server_down = server.try_clone().expect("clone server socket");
                            // server -> client is the download direction, and
                            // the only one on the metered side of the link.
                            let throttled = thread::spawn(move || {
                                relay(server_down, client_down, Some(limiter))
                            });
                            relay(client, server, None);
                            drop(throttled.join());
                        });
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            stop,
            accept: Some(accept),
            connections,
        }
    }

    fn connections(&self) -> u64 {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for ThrottledProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            drop(accept.join());
        }
    }
}

// ---------------------------------------------------------------------------
// The remote: an sshd in a temp dir, with throwaway keys
// ---------------------------------------------------------------------------

fn keygen(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-f")
        .arg(path)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed for {}", path.display());
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// A temp dir with a **short** path, which this measurement needs and a real
/// machine has for free.
///
/// The ssh client the sftp backend drives keeps its `ControlMaster` socket at
/// `<XDG_STATE_HOME>/.ssh-connection<rand>/master.<rand>`, and a unix socket
/// path may not exceed 104 bytes (`sockaddr_un.sun_path`). `tempfile`'s default
/// root is `/var/folders/<2>/<32>/T/` on macOS, which leaves too little room
/// and makes ssh fail with `unix_listener: path ... too long for Unix domain
/// socket` — a property of this sandbox's path, not of the code under test, and
/// one a user with `XDG_STATE_HOME=~/.local/state` never meets. So the sandbox
/// is rooted where a short path is available instead.
fn short_tempdir() -> tempfile::TempDir {
    let base = Path::new("/tmp");
    if base.is_dir() {
        return tempfile::Builder::new()
            .prefix("w246-")
            .tempdir_in(base)
            .expect("tempdir in /tmp");
    }
    tempfile::tempdir().expect("tempdir")
}

fn wait_until_listening(port: u16, log: &Path) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let log = fs::read_to_string(log).unwrap_or_else(|e| format!("<unreadable: {e}>"));
    panic!("sshd never listened on 127.0.0.1:{port}; its log:\n{log}");
}

/// A throwaway OpenSSH server: its own host key, its own client key, its own
/// port, its own empty remote root, all inside one temp dir it owns.
///
/// `StrictModes no` is why the copied `authorized_keys` needs no `chmod`: the
/// file is 0644 like the `.pub` it came from, which is a mode sshd would
/// otherwise refuse. Nothing here is reachable from outside loopback, and the
/// keys are deleted with the temp dir.
struct SftpServer {
    dir: tempfile::TempDir,
    port: u16,
    child: Child,
}

impl SftpServer {
    fn start() -> Self {
        assert!(
            Path::new(SSHD).exists(),
            "this measurement drives a real sshd and needs {SSHD}"
        );
        let dir = tempfile::tempdir().expect("sshd tempdir");
        let base = dir.path();
        keygen(&base.join("host_ed25519"));
        keygen(&base.join("user_ed25519"));
        fs::copy(base.join("user_ed25519.pub"), base.join("authorized_keys"))
            .expect("authorized_keys");
        let port = free_port();
        let config = base.join("sshd_config");
        fs::write(
            &config,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {}/host_ed25519\n\
                 PidFile {}/sshd.pid\n\
                 AuthorizedKeysFile {}/authorized_keys\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\n\
                 UsePAM no\n\
                 StrictModes no\n\
                 Subsystem sftp internal-sftp\n",
                base.display(),
                base.display(),
                base.display(),
            ),
        )
        .expect("write sshd_config");
        let log = base.join("sshd.log");
        let child = Command::new(SSHD)
            .arg("-f")
            .arg(&config)
            .args(["-D", "-e"])
            .stderr(fs::File::create(&log).expect("sshd log"))
            .spawn()
            .expect("spawn sshd");
        wait_until_listening(port, &log);
        Self { dir, port, child }
    }

    /// The client's private key, and the remote directory the repository lives
    /// in. Both are absolute paths on this machine, which is what a remote
    /// path is from the server's own point of view.
    fn client_key(&self) -> PathBuf {
        self.dir.path().join("user_ed25519")
    }

    fn remote_root(&self) -> PathBuf {
        self.dir.path().join("repo")
    }
}

impl Drop for SftpServer {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

// ---------------------------------------------------------------------------
// The fixture and the measured runs
// ---------------------------------------------------------------------------

fn cfg(repo_root: &str, key: &Path, options: BTreeMap<String, String>) -> StoreConfig {
    StoreConfig {
        repo_root: repo_root.to_string(),
        key_file: key.to_path_buf(),
        connections: 1,
        options,
        cache_dir: None,
        no_cache: false,
    }
}

fn session_id(n: u32) -> String {
    format!("claude-code.m-alpha.aaaaaaaa-0000-0000-0000-{n:012}")
}

/// A deterministic shard body built to be *hard* to compress: an LCG-derived
/// alphabet soup. Identical in construction to W243's, so the two fixtures are
/// the same size and shape and their numbers are comparable.
///
/// Measured, it is not incompressible — the 62-character alphabet is drawn
/// uniformly, so the stream carries 5.95 bits per byte and zstd reaches 0.746
/// of its length, within a point of the 0.744 entropy floor. W243's note calls
/// this fixture "incompressible"; that is an overstatement, and the number it
/// affects is the one this file reports as **link** bytes rather than the
/// wall-clock medians.
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

/// Everything one run of the measurement needs.
///
/// Field order is drop order, and it is deliberate: the ssh server and the
/// proxy are torn down *before* the temp dir they serve out of, so the dir is
/// never deleted while a live sshd still has its key, its config and the
/// repository open. The two are independent — the repository lives under the
/// server's own temp dir — but a teardown that reads front-to-back should not
/// have to know that.
struct Sandbox {
    server: SftpServer,
    proxy: ThrottledProxy,
    limiter: Arc<Limiter>,
    key: PathBuf,
    stage: PathBuf,
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = short_tempdir();
        let root = dir.path();
        let stage = root.join("stage");
        fs::create_dir_all(&stage).expect("stage dir");
        // ssh wants a HOME; StrictHostKeyChecking=no means it never writes a
        // known_hosts, but it still looks for ~/.ssh.
        fs::create_dir_all(root.join("home").join(".ssh")).expect("home/.ssh");

        let server = SftpServer::start();
        let limiter = Arc::new(Limiter::new(DOWNLOAD_BYTES_PER_SEC));
        let proxy = ThrottledProxy::start(server.port, limiter.clone());

        // One session spread over many shards, exactly as W243's local fixture:
        // enough shards that the destination's drawn chunking polynomial is
        // defeated, which is the shape a session that accrues many collects has.
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

        let key = root.join("key.json");
        let mk = rustic_core::repofile::MasterKey::new();
        store::persist_key_file(&cfg("opendal:sftp", &key, BTreeMap::new()), &mk)
            .expect("persist key");
        let push_cfg = cfg(
            "opendal:sftp",
            &key,
            Self::backend_options(&proxy, &server.client_key(), &server.remote_root()),
        );
        let store = BackupStore::new(push_cfg, MACHINE.to_string());
        assert!(
            store.push(&stage, &mk).expect("push").files_new > 0,
            "the fixture must actually archive something"
        );

        Self {
            server,
            proxy,
            limiter,
            key,
            stage,
            dir,
        }
    }

    /// The destination's backend options: the same four keys a user's
    /// `[destinations.x.options]` table carries, pointed at the proxied port so
    /// the production `opendal:sftp` operator dials the metered link.
    fn backend_options(
        proxy: &ThrottledProxy,
        key: &Path,
        remote_root: &Path,
    ) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "endpoint".to_string(),
                format!("ssh://127.0.0.1:{}", proxy.port),
            ),
            (
                "user".to_string(),
                std::env::var("USER").unwrap_or_else(|_| "nobody".to_string()),
            ),
            ("key".to_string(), key.display().to_string()),
            // The host key is generated per run and thrown away with the temp
            // dir, so there is nothing to pin it against.
            ("known_hosts_strategy".to_string(), "accept".to_string()),
            ("root".to_string(), remote_root.display().to_string()),
        ])
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
    /// is a true cold first-read that must fetch every body block over the link.
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
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    /// `read --session 0` against the sftp destination behind the proxy;
    /// returns (elapsed_ms, bytes the link carried, ssh connections opened, output).
    fn read_once(&self) -> (u128, u64, u64, Output) {
        let mut command = self.command();
        command
            .arg("read")
            .arg("--repo")
            .arg("opendal:sftp")
            .arg("--key-file")
            .arg(&self.key)
            .arg("--stage")
            .arg(&self.stage)
            .arg("--machine")
            .arg(MACHINE)
            .arg("--session")
            .arg(session_id(0));
        for (k, v) in Self::backend_options(
            &self.proxy,
            &self.server.client_key(),
            &self.server.remote_root(),
        ) {
            command.arg("--option").arg(format!("{k}={v}"));
        }
        let before = self.limiter.charged();
        let before_connections = self.proxy.connections();
        let start = Instant::now();
        let out = command.output().expect("run read");
        let ms = start.elapsed().as_millis();
        (
            ms,
            self.limiter.charged() - before,
            self.proxy.connections() - before_connections,
            out,
        )
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Both streams, for the failure messages: a read that stops early says why on
/// stderr, and an assertion that printed only stdout would hide it.
fn both(output: &Output) -> String {
    format!(
        "{}\n--- stderr ---\n{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    )
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

/// The middle sample. Generic over the two things sampled here — elapsed
/// milliseconds and bytes carried — so both medians are the same rule.
fn median<T: Copy + Ord>(mut values: Vec<T>) -> T {
    values.sort_unstable();
    values[values.len() / 2]
}

const SESSION_BYTES: u64 = SHARD_BYTES as u64 * SHARD_COUNT as u64;

#[test]
#[ignore = "manual remote-like measurement; starts sshd and moves ~100 MB per run"]
fn measure_100mb_session_cold_vs_warm_over_a_throttled_sftp_link() {
    let sandbox = Sandbox::new();
    // The cache must hold the whole ~102 MB session, and a session no larger
    // than a tenth of the quota may be stored: 2 GiB quota → 10% = 215 MiB,
    // comfortably above it.
    sandbox.set_quota("2GiB");

    println!(
        "archive: {SESSION_BYTES} B plaintext ({:.0} MB) over {SHARD_COUNT} shards",
        SESSION_BYTES as f64 / 1e6
    );
    println!(
        "destination: opendal:sftp via ssh -> a TCP proxy limiting downloads to {:.2} MB/s \
         (W117's 107 MB / 22.6 s)",
        DOWNLOAD_BYTES_PER_SEC / 1e6
    );
    println!("cache: quota 2GiB (10% = 215 MiB > session, so the session may be stored)");
    println!(
        "{:<6} {:<6} {:>8} {:>12} {:>6} {:>6} {:>6}",
        "run", "kind", "ms", "link bytes", "ssh", "hits", "misses"
    );

    let mut cold_ms = Vec::new();
    let mut cold_bytes = Vec::new();
    let mut cold_ssh = Vec::new();
    for i in 0..SAMPLES {
        sandbox.clear_cache();
        let (ms, bytes, ssh, out) = sandbox.read_once();
        assert_eq!(out.status.code(), Some(0), "{}", both(&out));
        // The limiter is only a limiter if the read went through it: a cold
        // read of an unchanged session has to pull the session's ciphertext
        // over the link, and if it did not, every number below is measuring
        // loopback. Half the plaintext is a floor no compression of this
        // fixture could reach, so the bound cannot be met by accident.
        assert!(
            bytes >= SESSION_BYTES / 2,
            "a cold read must carry the session across the metered link; \
             it moved {bytes} B of {SESSION_BYTES} B"
        );
        println!(
            "{:<6} {:<6} {:>8} {:>12} {:>6} {:>6} {:>6}",
            i + 1,
            "cold",
            ms,
            bytes,
            ssh,
            cache_stat(&out, "hits"),
            cache_stat(&out, "misses"),
        );
        cold_ms.push(ms);
        cold_bytes.push(bytes);
        cold_ssh.push(ssh);
    }

    // The cache is already hot from the last cold read; this sample just
    // confirms the warm path works and pins the reference digest.
    let (_, _, _, fill) = sandbox.read_once();
    assert_eq!(fill.status.code(), Some(0), "{}", both(&fill));
    let reference = concat_sha(&fill);

    let mut warm_ms = Vec::new();
    let mut warm_bytes = Vec::new();
    let mut warm_ssh = Vec::new();
    for i in 0..SAMPLES {
        let (ms, bytes, ssh, out) = sandbox.read_once();
        assert_eq!(out.status.code(), Some(0), "{}", both(&out));
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
        // Corpus of the same claim, measured where it counts: the link. A
        // warm read may still exchange SFTP chatter and a little metadata; it
        // must not move a body.
        assert!(
            bytes < SESSION_BYTES / 10,
            "a warm read must not download the session: it moved {bytes} B"
        );
        println!(
            "{:<6} {:<6} {:>8} {:>12} {:>6} {:>6} {:>6}",
            i + 1,
            "warm",
            ms,
            bytes,
            ssh,
            cache_stat(&out, "hits"),
            cache_stat(&out, "misses"),
        );
        warm_ms.push(ms);
        warm_bytes.push(bytes);
        warm_ssh.push(ssh);
    }

    let c = median(cold_ms.clone());
    let w = median(warm_ms.clone());
    let c_bytes = median(cold_bytes.clone());
    let w_bytes = median(warm_bytes.clone());
    println!(
        "median cold = {c} ms (link {:.1} MB, {} ssh connections) · \
         median warm = {w} ms (link {:.2} MB, {} ssh connections) · warm {:.1}% of cold",
        c_bytes as f64 / 1e6,
        median(cold_ssh.clone()),
        w_bytes as f64 / 1e6,
        median(warm_ssh.clone()),
        (w as f64 * 100.0) / c as f64
    );
    // The two numbers the interpretation rests on, printed rather than left to
    // be re-derived by eye: how long the metered transfer itself accounts for,
    // and therefore how much of each read is *not* the link.
    println!(
        "metered transfer at {:.2} MB/s: cold {:.1} s of {:.1} s · warm {:.2} s of {:.1} s",
        DOWNLOAD_BYTES_PER_SEC / 1e6,
        c_bytes as f64 / DOWNLOAD_BYTES_PER_SEC,
        c as f64 / 1000.0,
        w_bytes as f64 / DOWNLOAD_BYTES_PER_SEC,
        w as f64 / 1000.0
    );
    println!("cold runs: {cold_ms:?}\nwarm runs: {warm_ms:?}");
    println!("cold link bytes: {cold_bytes:?}\nwarm link bytes: {warm_bytes:?}");
    println!("cold ssh connections: {cold_ssh:?}\nwarm ssh connections: {warm_ssh:?}");
}
