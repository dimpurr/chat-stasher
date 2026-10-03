//! Fixture plumbing shared by this crate's unit tests and the integration
//! suites.
//!
//! The crate's unit tests cannot see a module owned by `tests/`, and
//! integration tests cannot see a `#[cfg(test)]` item of the crate, so the
//! integration suites pull this file in with `#[path]` rather than keeping a
//! second copy that would drift out of step with this one.
//!
//! Everything here exists because of one kernel rule: `execve` refuses a file
//! that any process holds open for writing, reporting ETXTBSY ("Text file
//! busy", os error 26). A descriptor this process opens is inherited by every
//! child forked while it is still open, and that child keeps it until its own
//! `execve`. `O_CLOEXEC` cannot close that window, because the window sits
//! *between* `fork` and `exec`. So a fixture written here and executed a moment
//! later can be refused because of a child some *other* test thread forked in
//! between — with the default one-thread-per-test runner, a write-then-exec
//! fixture is a race even though the test that owns it is single-threaded.
//!
//! ```text
//! thread A:  open(fixture, O_WRONLY) … write … close
//! thread B:               fork ──────────► child inherits A's write descriptor
//! thread A:                              exec(fixture) ──► ETXTBSY
//! ```
//!
//! Planting the bytes from a child process keeps the descriptor out of this
//! process entirely, so no fork of ours can carry it and the window above
//! cannot exist at all. That is why this is a write-side fix and not a retry:
//! a retry would only make the window shorter, and would have to guess how
//! long another thread's child takes to reach its own `execve`.
#![allow(dead_code)] // the integration suites also use `copy_executable`

/// One per-test temporary root, and the environment that points every real
/// user-data location at it.
///
/// This is the W306 shared fixture. A test — or a binary it spawns — that gets
/// its environment from [`Sandbox::apply`] cannot reach the machine's real
/// config, data, state, cache, native-messaging manifest directories or inbox,
/// because every one of them resolves through a variable this sets:
///
/// | location | variable |
/// |---|---|
/// | home (and Windows' fallback) | `HOME` / `USERPROFILE` |
/// | config (`config_path`) | `XDG_CONFIG_HOME` |
/// | data + state (`default_data_root`, `default_state_dir`) | `XDG_DATA_HOME` |
/// | scanner's state home | `XDG_STATE_HOME` |
/// | cache (`dirs::cache_dir`) | `XDG_CACHE_HOME` |
/// | rustic's metadata cache | [`RUSTIC_CACHE_DIR_ENV`] |
///
/// The native-messaging manifest dirs are under `HOME` on macOS/Linux and the
/// Known Folder API on Windows; the latter is not environment-redirectable, so
/// manifest tests pass an explicit `--target-root` instead — see
/// `tests/nativehost_install_test.rs`. `apply` still moves `HOME` for the
/// mac/linux half.
///
/// The rustic cache pin travels through the product's own knob, not
/// `XDG_CACHE_HOME`, for the same Windows reason W289 documents.
pub struct Sandbox {
    dir: tempfile::TempDir,
    marker: Option<String>,
}

/// The per-run isolation marker `check-test-isolation.sh` exports
/// (W306b).
///
/// It is a random token the guard generates for one run and hands to the whole
/// process tree. A test write is only *provably* a test write if it carries
/// something the live product never writes, and the marker is that something:
/// the guard scans the real roots for it and reds when it finds it, while the
/// live product — which never sees the variable — cannot trip that scan no
/// matter how often it rewrites the real stage.
pub const TEST_MARKER_ENV: &str = "CHAT_STASHER_TEST_ISOLATION_MARKER";

/// Name of the file [`Sandbox::new`] writes into its root, holding the marker.
/// The path itself carries it too, so a value the product records *by path*
/// (a status record, an audit row) is marked even when the file lands
/// elsewhere.
pub const TEST_MARKER_FILE: &str = ".chat-stasher-test-isolation-marker";

impl Sandbox {
    /// A fresh sandbox. The directory is created; its subdirectories are
    /// created lazily by [`Sandbox::ensure_dirs`] or by the code under test.
    ///
    /// When the isolation guard is running, the sandbox names itself after the
    /// run marker: every path it hands a test carries the marker, so a leaked
    /// write that echoes a sandbox path is caught by the guard's fingerprint
    /// scan. Outside the guard the marker is absent and the root is an ordinary
    /// temp directory.
    pub fn new() -> Sandbox {
        let marker = std::env::var(TEST_MARKER_ENV)
            .ok()
            .filter(|marker| !marker.is_empty());
        let prefix = match &marker {
            Some(marker) => format!("cs-sandbox-{marker}-"),
            None => "cs-sandbox-".to_string(),
        };
        let dir = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .expect("create the per-test sandbox");
        if let Some(marker) = &marker {
            // Best-effort: the marker file is a convenience for a copy or
            // rename of the tree. The path already carries the marker; a write
            // failure here must not fail a test that never needed the file.
            drop(std::fs::write(dir.path().join(TEST_MARKER_FILE), marker));
        }
        Sandbox { dir, marker }
    }

    /// The run marker, when the isolation guard set one.
    pub fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    /// The sandbox root. Every other path is under it.
    pub fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    pub fn home(&self) -> std::path::PathBuf {
        self.root().join("home")
    }

    pub fn config_home(&self) -> std::path::PathBuf {
        self.root().join("config")
    }

    pub fn data_home(&self) -> std::path::PathBuf {
        self.root().join("data")
    }

    pub fn state_home(&self) -> std::path::PathBuf {
        self.root().join("state")
    }

    pub fn cache_home(&self) -> std::path::PathBuf {
        self.root().join("cache")
    }

    /// [`rustic_cache_root`] rooted at this sandbox, kept as one spelling.
    pub fn rustic_cache_dir(&self) -> std::path::PathBuf {
        rustic_cache_root(self.root())
    }

    /// Create the `home`, `config`, `data`, `state` and `cache` directories.
    /// Spawning a child that takes the stage lock needs its `home` to exist,
    /// and a config-writing test needs its config directory to exist.
    pub fn ensure_dirs(&self) {
        for path in [
            self.home(),
            self.config_home(),
            self.data_home(),
            self.state_home(),
            self.cache_home(),
        ] {
            std::fs::create_dir_all(&path)
                .unwrap_or_else(|e| panic!("create sandbox dir {}: {e}", path.display()));
        }
    }

    /// The environment this sandbox pins, as `(name, value)` pairs.
    ///
    /// Prefer [`Sandbox::apply`]. This is `pub` so a test that builds an
    /// environment map by hand (the Node e2e harness is not Rust, but a Rust
    /// test assembling `Command::envs` can be) draws from the same list rather
    /// than a second spelling that drifts.
    pub fn envs(&self) -> Vec<(&'static str, std::ffi::OsString)> {
        let mut envs = vec![
            ("HOME", self.home().into()),
            ("USERPROFILE", self.home().into()),
            ("XDG_CONFIG_HOME", self.config_home().into()),
            ("XDG_DATA_HOME", self.data_home().into()),
            ("XDG_STATE_HOME", self.state_home().into()),
            ("XDG_CACHE_HOME", self.cache_home().into()),
            (RUSTIC_CACHE_DIR_ENV, self.rustic_cache_dir().into()),
        ];
        if let Some(marker) = &self.marker {
            // A spawned child keeps the marker for the same reason this
            // process has it: whatever it derives from its environment is
            // traceable to this run.
            envs.push((TEST_MARKER_ENV, marker.into()));
        }
        envs
    }

    /// Point `command` at this sandbox: set every variable in
    /// [`Sandbox::envs`]. Returns the command for chaining.
    pub fn apply<'a>(
        &self,
        command: &'a mut std::process::Command,
    ) -> &'a mut std::process::Command {
        for (name, value) in self.envs() {
            command.env(name, value);
        }
        command
    }
}

/// The product's runtime override for its rustic metadata cache
/// (`chat_stasher::config::rustic_cache_dir`), re-stated here so an
/// integration suite can name it through the one fixture it already imports.
///
/// This file is compiled twice — once as a private module of the library and
/// once, via `#[path]`, inside an integration-test crate — and the two do not
/// share a path to the constant (the test crate knows the library only as
/// `chat_stasher`, and the library cannot name itself that way inside its own
/// `--test` build). The copy is kept honest by
/// `config::tests::test_support_restates_the_cache_dir_env_name`, which fails
/// the moment the two spell the variable differently.
pub const RUSTIC_CACHE_DIR_ENV: &str = "CHAT_STASHER_RUSTIC_CACHE_DIR";

/// The rustic metadata-cache **root** a spawned child should use, rooted in
/// the test's own sandbox: `<sandbox>/cache/rustic`.
///
/// Every test that spawns `chat-stasher` sets
/// [`RUSTIC_CACHE_DIR_ENV`] to this. It has to travel through the product's
/// config knob rather than `XDG_CACHE_HOME`/`HOME`, because on Windows rustic
/// resolves its cache root through the Known Folder API
/// (`dirs-6.0.0` `src/win.rs:10` → `known_folder_local_app_data`) and no
/// environment variable redirects that; `HOME` and `XDG_CACHE_HOME` are still
/// set by the callers for the *other* paths they isolate.
///
/// The trailing `rustic` component is not decoration: the suite's
/// tree-snapshot comparisons recognise "the library's incidental cache" by a
/// `rustic` path component, and keeping that name here is what lets those
/// comparisons stay a single spelling on every platform (W289).
pub fn rustic_cache_root(sandbox: &std::path::Path) -> std::path::PathBuf {
    sandbox.join("cache").join("rustic")
}

/// Names a Grok Bot persistence state key the way the app names its blobs on
/// disk: the unpadded, lowercase RFC 4648 base32 of the UTF-8 key, plus a
/// `.blob` suffix (W321 measured the shape locally). Production code only
/// *decodes* these names, so the inverse lives here, for fixtures only —
/// every key a test passes must be synthetic.
pub fn grok_bot_blob_name(state_key: &str) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut buffer = 0u16;
    let mut bits = 0u16;
    let mut encoded = String::with_capacity(state_key.len() * 8 / 5 + 3);
    for byte in state_key.as_bytes() {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            encoded.push(ALPHABET[usize::from((buffer >> bits) & 31)] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        encoded.push(ALPHABET[usize::from((buffer << (5 - bits)) & 31)] as char);
    }
    encoded.to_ascii_lowercase()
}

/// Writes `body` to `path` as an executable script, without this process ever
/// holding a descriptor open on `path`.
///
/// The bytes travel to a child `sh` through the environment and are written by
/// that shell's own redirection, so `path` is opened for writing only there.
/// The mode is set here afterwards: `chmod` is a path operation, and a path
/// operation opens no descriptor.
#[cfg(unix)]
pub fn plant_executable(path: &std::path::Path, body: &str) {
    use std::process::Command;

    let status = Command::new("sh")
        .arg("-c")
        .arg("printf '%s' \"$CHAT_STASHER_FIXTURE_BODY\" > \"$1\"")
        .arg("sh")
        .arg(path)
        .env("CHAT_STASHER_FIXTURE_BODY", body)
        .status()
        .expect("run a child shell to plant the fixture");
    assert!(
        status.success(),
        "the child shell could not plant the fixture at {}",
        path.display()
    );
    set_executable(path);
}

/// Copies `source` to `destination` and makes it executable, again leaving no
/// descriptor open on `destination` in this process.
#[cfg(unix)]
pub fn copy_executable(source: &std::path::Path, destination: &std::path::Path) {
    use std::process::Command;

    let status = Command::new("cp")
        .arg(source)
        .arg(destination)
        .status()
        .expect("run cp to place the fixture");
    assert!(
        status.success(),
        "cp could not place the fixture at {}",
        destination.display()
    );
    set_executable(destination);
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make the fixture executable");
}
