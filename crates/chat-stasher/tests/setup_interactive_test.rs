//! The wizard's interactive declaration prompt, driven the way a user drives
//! it: a real terminal, the real binary, an answer typed at the prompt it
//! printed.
//!
//! W284's second finding. The declined-prompt fix landed with a unit test that
//! handed `Declined` to `setup_declaration_from_answer` — the seam the fix
//! itself introduced — and such a test cannot fail before the fix, because
//! neither the enum nor the seam exists there; the function the bug actually
//! lived in, the printed prompt and its return value, was never exercised. A
//! regression test that cannot run against the unfixed code pins nothing.
//!
//! The bug being pinned, exactly: before that fix, the prompt's return value
//! was `!declared || record_key_declaration(...)`, which is `true` for a
//! declined answer — so an interactive run on a fresh machine said "NOT
//! declared" on stderr and then *reported the step as done*: it finished with
//! exit `0`, `missing_parameters` empty, and nothing recorded. This suite runs
//! that flow through the real interactive path and demands the opposite —
//! exit `2`, the refusal said in the wizard's own words, and no record on the
//! disk. It fails on the code the fix replaced; that is the test W284 asked
//! for.
//!
//! The interactive path needs `stdin().is_terminal() && stdout().is_terminal()`
//! — a pipe will not do on any platform — so this suite allocates a
//! pseudo-terminal and runs the real binary with its stdin and stdout on the
//! slave side, stderr piped so the assertions can quote the wizard's own
//! words. Every prompt is waited for before it is answered: the transcript is
//! in every failure message, so a hang reports itself rather than timing the
//! suite out.
//!
//! Every test in this file runs in the same process, on the runner's default
//! one-thread-per-test schedule, so the allocation below is shared code under
//! real concurrency. W300 is what that cost on the glibc cells: the libc call
//! the allocation used (`ptsname`) answers out of a buffer the whole process
//! shares there, so two tests starting at once could be handed one terminal —
//! and the test whose master then had no slave at all blocked in `read` until
//! the 120s deadline, reporting an empty transcript. [`allocate_pty`] is the
//! one place a terminal is allocated, and it is serialised; the last test in
//! this file checks that the path it hands the wizard is the kernel's own
//! answer for that master.
//!
//! W348 is the second face of the same empty transcript, and it is not about
//! allocation at all: a pty discards whatever the reader has not drained when
//! the last slave descriptor closes. The child's own descriptors close when it
//! exits, so a reader that has not reached its first `read` yet — a real window
//! under a loaded runner, even with the reader started before the child — can
//! find the buffer already gone. So this suite plays the terminal emulator
//! rather than the child's parent: [`ParentTerminal`] opens the slave once and
//! holds that descriptor for the whole run, exactly as a real emulator does,
//! which is what keeps the tty — and its buffer — alive after the child is gone.
//!
//! Holding it settles the order the two ends are released in, and the kernel
//! does not forgive the other order: draining has to happen before the parent
//! lets go, because closing the last slave is what ends the stream, and the
//! unread buffer goes down with it.
//! [`a_reader_that_starts_after_the_child_exited_still_drains_the_transcript`]
//! pins that where it is observed — the transcript this suite's own `finish`
//! returns, read by a reader started after the child exited, which is the
//! window rather than a race the test might win — and
//! [`a_kept_terminal_still_holds_output_written_before_the_child_exited`] pins
//! the mechanism underneath it with a direct read.
//!
//! None of that rests on starting the reader before the child, so the harness
//! does not claim it: a signal from the reader's own thread would only say the
//! thread was scheduled, not that it was inside `read`. Every run therefore
//! prints a sentinel from a throwaway child on the same terminal first, and the
//! wizard starts only once the reader has reported seeing it — which it can
//! only have done from inside its read loop.
//!
//! `#[cfg(unix)]` states the platform gap rather than hiding it: the mechanism
//! is `posix_openpt`, which has no counterpart in this workspace's
//! dependencies — Windows needs ConPTY, an FFI surface this repo has no
//! bindings for and that the machine writing these tests could not exercise,
//! and untestable FFI in a test would turn the Windows CI cell red for the
//! test's construction rather than for the product. The cross-platform halves
//! of the same property run in `setup_cli_test.rs` everywhere: a non-
//! interactive run that owes the declaration exits `2`, and the recorded
//! declaration is what clears it. The interactive decline itself is covered
//! on Unix; that is a real gap on Windows, stated here rather than papered
//! over.
//!
//! Every fixture is synthetic: one opaque JSONL line the collector can count,
//! a throwaway HOME, and a registry narrowed to one harness, so nothing here
//! reads a session body, a key file's contents, or anything on the machine
//! that runs it.

#[cfg(unix)]
use std::ffi::CStr;

#[path = "../src/test_support.rs"]
mod test_support;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::mpsc::RecvTimeoutError;
#[cfg(unix)]
use std::sync::{mpsc, Arc, Barrier, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// Generous, because the wizard's local first save does real archive work
/// between prompts; what matters is that a wait never hangs the suite — every
/// wait ends in either a found prompt or a panic that prints the transcript.
#[cfg(unix)]
const WAIT: Duration = Duration::from_secs(120);

/// How long the terminal must print nothing before what the reader has is
/// treated as the whole of it.
///
/// A run cannot wait for the end of the stream to know that, because the parent
/// is holding the terminal open ([`ParentTerminal`]) and there is nothing left to
/// close it until the run is over. Silence is the substitute, and it is only
/// sound because the callers are past the child's exit: nothing more can be
/// written, so a quiet period means the reader has caught up. Generous, because
/// the alternative is a busy runner's scheduling gap reading as the end of the
/// wizard's output; four interactive tests pay it once each.
#[cfg(unix)]
const QUIET: Duration = Duration::from_secs(1);

/// What a run prints from a throwaway child to prove the reader is draining
/// before the wizard can print. Never answered and never read: it exists to be
/// seen by the reader, and is discarded rather than added to any transcript.
#[cfg(unix)]
const READER_READY_SENTINEL: &str = "w348-reader-ready";

/// What the late-reader regression test's child prints, standing in for a
/// wizard's output.
#[cfg(unix)]
const LATE_READER_SENTINEL: &str = "w348-late-reader";

/// The bundled registry narrowed to `claude-code`, for the same reason
/// `setup_cli_test.rs` narrows it: a test whose session count depends on what
/// is installed where it runs is green here and red elsewhere.
#[cfg(unix)]
fn claude_code_only_registry() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("harness-registry-v1.json");
    let text = fs::read_to_string(&path).expect("read the bundled harness registry");
    let mut registry: serde_json::Value =
        serde_json::from_str(&text).expect("the bundled registry is JSON");
    let harnesses = registry["harnesses"]
        .as_array_mut()
        .expect("the registry has a harnesses array");
    harnesses.retain(|harness| harness["id"] == "claude-code");
    assert_eq!(harnesses.len(), 1, "claude-code must be in the registry");
    serde_json::to_string(&registry).expect("serialise the narrowed registry")
}

/// A throwaway machine for one interactive run: isolated HOME, isolated XDG
/// tree, its own registry, and one synthetic session for the first archive
/// pass to find.
#[cfg(unix)]
struct Sandbox {
    root: tempfile::TempDir,
}

#[cfg(unix)]
impl Sandbox {
    fn new() -> Self {
        let sandbox = Sandbox {
            root: tempfile::tempdir().expect("create a temp dir"),
        };
        for dir in ["home", "data", "config", "state"] {
            fs::create_dir_all(sandbox.root.path().join(dir)).expect("create the sandbox tree");
        }
        fs::write(
            sandbox.root.path().join("registry.json"),
            claude_code_only_registry(),
        )
        .expect("write the sandbox registry");
        let sessions = sandbox
            .home()
            .join(".claude")
            .join("projects")
            .join("fixture-project");
        fs::create_dir_all(&sessions).expect("create the fixture harness root");
        fs::write(
            sessions.join("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl"),
            b"{\"note\":\"interactive-path synthetic session line, not a conversation\"}\n",
        )
        .expect("write the synthetic session");
        sandbox
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn stage(&self) -> PathBuf {
        self.home().join("stash").join("chat-stasher").join("stage")
    }

    fn data_root(&self) -> PathBuf {
        self.root.path().join("data").join("chat-stasher")
    }

    /// The state directory the wizard writes its own records to
    /// (`$XDG_DATA_HOME/chat-stasher/state`), which is where a key declaration
    /// is recorded and read back from.
    fn state_dir(&self) -> PathBuf {
        self.data_root().join("state")
    }

    fn config_file(&self) -> PathBuf {
        self.root
            .path()
            .join("config")
            .join("chat-stasher")
            .join("config.toml")
    }

    /// Declare a destination by hand, the way a user who already has one does.
    /// An ordinary local-path repository, so the destination step runs in the
    /// same process with no network.
    fn write_config(&self, text: &str) {
        let path = self.config_file();
        fs::create_dir_all(path.parent().expect("a config directory")).expect("create config dir");
        fs::write(&path, text).expect("write the sandbox config");
    }

    /// Run the documented first `setup` non-interactively, so the keys it names
    /// are on the disk before an interactive run.
    ///
    /// A key a run creates is never declared by that run, so an interactive run
    /// that has to reach a declaration *prompt* needs the key to pre-date it: a
    /// key it created itself is not asked about, it is named and left owed. With
    /// no `extra`, this creates the local repository and its key; naming a
    /// destination also creates that destination's own key, because the
    /// destination step runs inside the same bootstrap.
    fn bootstrap_keys(&self, extra: &[&str]) {
        let stage = self.stage();
        let mut args = vec![
            "setup",
            "--stage",
            stage.to_str().expect("a utf-8 stage path"),
        ];
        args.extend_from_slice(extra);
        let output = self
            .command(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run the non-interactive bootstrap");
        assert_eq!(
            output.status.code(),
            Some(2),
            "the bootstrap owes the masterkey declaration; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            self.data_root().join("masterkey.json").exists(),
            "the bootstrap must leave the local key on the disk"
        );
    }

    /// The command, environment and all, with stdio left to the caller: the
    /// interactive runs here put a terminal where the wizard's detection looks
    /// for one, which `Stdio::piped` never is.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .args(args)
            .env("HOME", self.home())
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&self.home()),
            )
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env(
                "CHAT_STASHER_REGISTRY",
                self.root.path().join("registry.json"),
            );
        command
    }
}

/// What the reading thread hands the driver: another chunk of terminal output,
/// or the stream ending (the child exited and closed its terminal side).
#[cfg(unix)]
enum Terminal {
    Chunk(Vec<u8>),
    End,
}

/// A `chat-stasher` run with its stdin and stdout on a real pseudo-terminal
/// and its stderr on a pipe, plus the accumulated transcript of everything the
/// terminal has printed.
#[cfg(unix)]
struct InteractiveWizard {
    child: std::process::Child,
    /// The pty master, write half: what is written here is what the wizard's
    /// stdin reads.
    master_write: fs::File,
    /// This suite's own hold on the terminal, as a terminal emulator holds one.
    /// It is what is still open when the wizard's descriptors are not, so it is
    /// what decides whether the wizard's output can still be read. See
    /// [`ParentTerminal`].
    parent_terminal: ParentTerminal,
    transcript: String,
    terminal: mpsc::Receiver<Terminal>,
}

/// The one lock this suite takes around the pty API, and the reason it exists.
///
/// `ptsname(3)` is not reentrant: with glibc it returns a pointer into a
/// **file-scope `static char buffer[]`** that the man page says is "valid until
/// the next call", so two threads asking at once can both be handed the *same*
/// slave path. The tests in this file run concurrently by default, each wizard
/// test allocating its terminal at start-up, and a crossing does not fail
/// loudly: both wizards run on one terminal and the test whose master has no
/// slave at all blocks in `read` forever — W300's flake, whose only symptom was
/// an empty transcript at the 120s deadline. Darwin's `ptsname` keeps its buffer
/// per thread, which is why this never reproduced on macOS and was seen exactly
/// once, on the ubuntu cell.
///
/// `libc` declares the reentrant `ptsname_r` for `linux_like` but not for
/// Apple, so the portable way to close the window is to hold this lock across
/// both the call *and* the copy out of its buffer. `allocate_pty` is the only
/// allocation in this process — nothing in the tool's own code calls `ptsname`
/// — so one lock is the whole of the exclusion this needs.
#[cfg(unix)]
static PTY_ALLOC: Mutex<()> = Mutex::new(());

/// Allocate one pseudo-terminal: the master's read and write halves, and the
/// slave's path as an owned `String`.
///
/// Safety: the `libc` calls here are the pty allocation dance on a fresh file
/// descriptor this test owns — `posix_openpt` to allocate, `grantpt`/`unlockpt`
/// to make the slave openable, `ptsname` for its path — each checked for
/// failure the same way `std::fs` would report it. The master fd is wrapped in a
/// `File` the moment `ptsname` has read its path, so nothing leaks. `ptsname`
/// hands back a pointer into a libc buffer; it is copied out into an owned
/// `String` here, and the whole of that is done under [`PTY_ALLOC`] because the
/// buffer is not this process's to keep — see that lock.
#[cfg(unix)]
fn allocate_pty() -> (fs::File, fs::File, String) {
    // A poisoned lock is not a second failure to report: the panic that poisoned
    // it happened inside this critical section and has already named the libc
    // call that failed. Reporting the poison instead would replace that name
    // with "poisoned" for the three tests that had nothing to do with it.
    let _allocating = PTY_ALLOC
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        panic!("posix_openpt failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::grantpt(master_fd) } != 0 {
        panic!("grantpt failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        panic!("unlockpt failed: {}", std::io::Error::last_os_error());
    }
    let slave_path = unsafe { libc::ptsname(master_fd) };
    if slave_path.is_null() {
        panic!("ptsname failed: {}", std::io::Error::last_os_error());
    }
    let slave_path = unsafe { CStr::from_ptr(slave_path) }
        .to_string_lossy()
        .into_owned();
    let master_read = unsafe { fs::File::from_raw_fd(master_fd) };
    let master_write = master_read
        .try_clone()
        .expect("clone the pty master for the writing half");
    (master_read, master_write, slave_path)
}

/// This suite's terminal, and the parent's own hold on it.
///
/// A pty is torn down when its last slave descriptor closes, and the kernel
/// throws away whatever the reader has not drained on the way down. The child's
/// descriptors close the moment it exits, so without a descriptor of the
/// parent's own the tty dies with the child and every byte the wizard printed is
/// lost — which is the empty transcript W300 and W348 both reported. A real
/// terminal emulator does not close its terminal when the program in it exits,
/// and neither does this one: the descriptor is opened once, before any child
/// runs, and held until the run has drained what it has to drain.
///
/// It is a *write* descriptor on purpose. Holding the slave's read side open
/// would be the same kind of hold on the tty, but it would also put a
/// descriptor on the child's stdin, and a wizard that has not been answered yet
/// must not see end-of-input it did not get. Every prompt in the tool is one
/// `read_line` and no loop on end-of-input, so this cannot hang one either way;
/// it is the narrower hold that is wanted.
#[cfg(unix)]
struct ParentTerminal {
    slave_path: String,
    /// Never read, only held: the descriptor is the whole point, and it is
    /// released when the struct is dropped. The leading underscore says so, the
    /// way the `_dir` and `_sandbox` fields in this suite's sibling tests say it
    /// for a temp directory kept alive for its side effect.
    _slave: fs::File,
}

#[cfg(unix)]
impl ParentTerminal {
    /// Take the parent's own descriptor on the terminal at `slave_path`.
    fn hold(slave_path: &str) -> Self {
        let slave = fs::OpenOptions::new()
            .write(true)
            .open(slave_path)
            .unwrap_or_else(|error| {
                panic!("open the pty slave for this suite's own terminal: {error}")
            });
        ParentTerminal {
            slave_path: slave_path.to_owned(),
            _slave: slave,
        }
    }

    /// Spawn `command` with stdin and stdout on this terminal and stderr piped.
    ///
    /// Two more opens of the slave path — read end for stdin, write end for
    /// stdout — because `Stdio` takes an owned file per stream. Both close in
    /// the parent as soon as `spawn` has duplicated them into the child; the
    /// one in [`ParentTerminal::hold`] does not, and that difference is the
    /// whole of W348.
    fn spawn(&self, mut command: Command) -> std::process::Child {
        let slave_stdin = fs::File::open(&self.slave_path)
            .unwrap_or_else(|error| panic!("open the pty slave for stdin: {error}"));
        let slave_stdout = fs::OpenOptions::new()
            .write(true)
            .open(&self.slave_path)
            .unwrap_or_else(|error| panic!("open the pty slave for stdout: {error}"));
        command
            .stdin(Stdio::from(slave_stdin))
            .stdout(Stdio::from(slave_stdout))
            .stderr(Stdio::piped());
        command
            .spawn()
            .unwrap_or_else(|error| panic!("run the child on the terminal: {error}"))
    }
}

/// Start the thread that drains a terminal's master into a channel, and hand the
/// channel back.
///
/// Nothing here knows about wizards. The thread owns the master read half for
/// as long as the terminal lasts and reports `Terminal::End` when that is over,
/// which is the one signal a run gets for "the terminal has nothing left" — and
/// only once this suite lets the terminal go ([`ParentTerminal`]).
#[cfg(unix)]
fn spawn_terminal_reader(master_read: fs::File) -> mpsc::Receiver<Terminal> {
    let (sender, terminal) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = master_read;
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    // End of the slave side: the child exited and this suite's
                    // own descriptor has been dropped (or the terminal broke).
                    // Both mean no more output; the driver's deadline,
                    // transcript, and exit status are what report the
                    // difference. A send that finds nobody listening is the
                    // driver being gone — not a failure worth reporting — so it
                    // is dropped rather than `let _`-ed.
                    drop(sender.send(Terminal::End));
                    return;
                }
                Ok(read) => {
                    if sender
                        .send(Terminal::Chunk(buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    terminal
}

/// A child that prints one line and exits, for the two places that need a
/// writer on the terminal that is not the wizard.
#[cfg(unix)]
fn sentinel_child(sentinel: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!("printf '{sentinel}\\n'"));
    command
}

#[cfg(unix)]
impl InteractiveWizard {
    /// Run the binary on the slave side of a freshly allocated terminal.
    fn run(sandbox: &Sandbox, args: &[&str]) -> Self {
        let (master_read, master_write, slave_path) = allocate_pty();
        // Held before anything runs on the terminal, so it also covers the
        // readiness probe below.
        let parent_terminal = ParentTerminal::hold(&slave_path);
        let terminal = spawn_terminal_reader(master_read);

        // Prove the reader is draining before the wizard can print. Sending a
        // signal from the reader's own thread would only say the thread was
        // scheduled — it can be descheduled again before it reaches `read`, and
        // the send is not a statement about where the thread is. A sentinel
        // printed by a child on this same terminal is the other kind of
        // evidence: the reader can only have reported it from inside its read
        // loop, and the wizard starts only afterwards.
        let mut probe = parent_terminal.spawn(sentinel_child(READER_READY_SENTINEL));
        probe.wait().expect("reap the readiness probe");
        discard_until_seen(&terminal, READER_READY_SENTINEL);

        let child = parent_terminal.spawn(sandbox.command(args));

        InteractiveWizard {
            child,
            master_write,
            parent_terminal,
            transcript: String::new(),
            terminal,
        }
    }

    /// Block until the terminal has printed `needle`, or fail with everything
    /// printed so far. Waiting for the prompt before typing is what makes this
    /// a driven interaction rather than a blind feed: an unexpected flow stops
    /// in the assertion, named, instead of the run waiting for an answer it
    /// never got.
    fn expect_within(&mut self, needle: &str, what: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.transcript.contains(needle) {
            // The parent keeps the terminal open, so a child that has already
            // exited is not a closed terminal: drain what it wrote and report,
            // rather than waiting out the deadline on a buffer that is already
            // complete.
            if let Some(status) = self.child.try_wait().expect("poll the wizard") {
                drain_until_quiet(&mut self.transcript, &self.terminal, QUIET);
                if self.transcript.contains(needle) {
                    return;
                }
                panic!(
                    "the wizard exited before printing {what} (`{needle}`); status: {status:?}; \
                     terminal transcript so far:\n{}",
                    self.transcript
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "the wizard did not print {what} (waited for `{needle}`) within {WAIT:?}; \
                     terminal transcript so far:\n{}",
                    self.transcript
                );
            }
            match self.terminal.recv_timeout(remaining) {
                Ok(Terminal::Chunk(chunk)) => {
                    self.transcript.push_str(&String::from_utf8_lossy(&chunk));
                }
                Ok(Terminal::End) | Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "the wizard closed its terminal output before printing {what} \
                         (`{needle}`); terminal transcript so far:\n{}",
                        self.transcript
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Type a line at the prompt: the newline is what makes the wizard's
    /// `read_line` return.
    fn answer(&mut self, line: &str) {
        let mut typed = String::from(line);
        typed.push('\n');
        self.master_write
            .write_all(typed.as_bytes())
            .expect("type the answer at the terminal");
    }

    /// Wait for the run to end, keeping the transcript flowing while it does.
    fn finish(mut self) -> (std::process::ExitStatus, String, String) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll the wizard") {
                break status;
            }
            if Instant::now() > deadline {
                let _killed = self.child.kill();
                panic!(
                    "the wizard did not exit within {WAIT:?}; terminal transcript so far:\n{}",
                    self.transcript
                );
            }
            match self.terminal.recv_timeout(Duration::from_millis(250)) {
                Ok(Terminal::Chunk(chunk)) => {
                    self.transcript.push_str(&String::from_utf8_lossy(&chunk));
                }
                Ok(Terminal::End)
                | Err(RecvTimeoutError::Timeout)
                | Err(RecvTimeoutError::Disconnected) => {}
            }
        };
        // The child is gone, so nothing more can be written to the terminal and
        // the reader has only the output it is holding left to hand over. The
        // parent still holds the terminal open, and that decides the order of
        // the next two steps: draining has to come first, because closing the
        // last slave descriptor is what ends the stream and the kernel discards
        // whatever is undrained as it does so. Letting go first would lose
        // exactly the bytes this harness exists not to lose — so the tty stays
        // open until the reader has been given its silence.
        drain_until_quiet(&mut self.transcript, &self.terminal, QUIET);
        let parent_terminal = self.parent_terminal;
        drop(parent_terminal);
        // Now the stream really does end, and the reader says so. This drain
        // returns on that end rather than on quiet.
        drain_until_quiet(&mut self.transcript, &self.terminal, QUIET);
        // Only then is stderr, which is finite now, read whole.
        let mut stderr = String::new();
        let mut pipe = self.child.stderr.take().expect("stderr was piped");
        pipe.read_to_string(&mut stderr)
            .expect("read the wizard's stderr");
        (status, stderr, self.transcript)
    }
}

/// Drain what the reader has queued into `transcript`, until the terminal
/// reports its end or `quiet` passes with nothing arriving.
///
/// The end of the stream is not something a run can wait for while it is still
/// going, because this suite holds the terminal open ([`ParentTerminal`]); the
/// quiet period is what stands in for it, and it is only sound where every
/// caller uses it: the child has already exited, so silence means the reader has
/// caught up rather than that the wizard is still thinking.
#[cfg(unix)]
fn drain_until_quiet(
    transcript: &mut String,
    terminal: &mpsc::Receiver<Terminal>,
    quiet: Duration,
) {
    let deadline = Instant::now() + quiet;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match terminal.recv_timeout(remaining) {
            Ok(Terminal::Chunk(chunk)) => {
                transcript.push_str(&String::from_utf8_lossy(&chunk));
            }
            Ok(Terminal::End)
            | Err(RecvTimeoutError::Timeout)
            | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Wait until the reader reports having read `needle`, then until the terminal
/// goes quiet, throwing away everything it reads.
///
/// The readiness probe is the only writer on the terminal at this point and it
/// has been reaped, so every byte it will ever produce is already in the pty or
/// already in the channel, and none of it may reach the transcript the wizard
/// assertions read. The quiet tail is what keeps its trailing `\r\n` — the
/// line discipline's `ONLCR` translation of the `\n` — out of that transcript
/// in the case where the newline arrived separately from the sentinel.
///
/// Panics rather than returning a verdict, because there is no verdict to
/// return: a run that starts its wizard without having seen this is a run whose
/// reader may never be proved to be draining, which is the thing this waits for.
#[cfg(unix)]
fn discard_until_seen(terminal: &mpsc::Receiver<Terminal>, needle: &str) {
    let deadline = Instant::now() + WAIT;
    let mut seen = String::new();
    let mut quiet_deadline: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let waiting = match quiet_deadline {
            Some(quiet_deadline) => quiet_deadline.saturating_duration_since(now),
            None => deadline.saturating_duration_since(now),
        };
        if waiting.is_zero() {
            if quiet_deadline.is_some() {
                return;
            }
            panic!(
                "the readiness probe's sentinel (`{needle}`) never came back through the reader \
                 within {WAIT:?}, so this run cannot claim a reader that was already draining \
                 before the wizard started; what the terminal printed instead:\n{seen}"
            );
        }
        match terminal.recv_timeout(waiting) {
            Ok(Terminal::Chunk(chunk)) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if quiet_deadline.is_none() && seen.contains(needle) {
                    quiet_deadline = Some(Instant::now() + QUIET);
                }
            }
            Ok(Terminal::End) => panic!(
                "the terminal ended before the readiness probe's sentinel (`{needle}`) reached \
                 the reader, so nothing was draining it; what it printed:\n{seen}"
            ),
            Err(RecvTimeoutError::Disconnected) => panic!(
                "the reader thread is gone before the readiness probe's sentinel (`{needle}`) \
                 reached it; what the terminal printed:\n{seen}"
            ),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// W284's second finding, pinned through the real interactive path: a user
/// who *declines* the declaration prompt has not made the declaration, and
/// the run must say the step is still owed rather than report it done.
///
/// On the code this regression was written against, this exact interaction
/// ended with exit `0` and `missing_parameters` empty — "NOT declared" on
/// stderr, "declared" in the run's own report — because the prompt's return
/// value was `true` for a declined answer. Every assertion below is on what
/// that run reports, so each one would have failed there: the exit code
/// demands `2`, the record demands never having been written, and the refusal
/// demands the wizard's own "NOT declared" sentence.
#[cfg(unix)]
#[test]
fn a_declined_prompt_leaves_the_step_owed_and_records_nothing() {
    let sandbox = Sandbox::new();
    // The key has to pre-date this run, or the wizard would not ask about it at
    // all: a key a run creates is named and left owed, never prompted for. The
    // decline this test is about is only reachable for a key that already
    // exists, so the documented first run puts it there.
    sandbox.bootstrap_keys(&[]);
    let stage = sandbox.stage();
    let mut wizard = InteractiveWizard::run(
        &sandbox,
        &[
            "setup",
            "--stage",
            stage.to_str().expect("a utf-8 stage path"),
        ],
    );

    // The wizard archives locally first, then shows the key and asks for the
    // declaration. The needle is the prompt's own sentence, so passing means
    // the run really did reach the interactive prompt rather than some other
    // flow.
    wizard.expect_within(
        "to declare you have your own copy",
        "the masterkey declaration prompt",
    );
    // Any answer that is not the exact sentence declines; this test types a
    // plain "no" so the decline could not be mistaken for the sentence.
    wizard.answer("no");

    // The run goes on asking about the destination and the scheduler; both
    // take the offered defaults, so no other step is owed.
    wizard.expect_within(
        "Destination name (blank to skip",
        "the destination-name prompt",
    );
    wizard.answer("");
    wizard.expect_within("Install the scheduler now?", "the scheduler prompt");
    wizard.answer("");

    let (status, stderr, transcript) = wizard.finish();

    // The refusal itself: the run stopped with `2`, the missing-parameter
    // code, because the only thing owed is the declaration the user declined.
    assert_eq!(
        status.code(),
        Some(2),
        "a declined declaration leaves the step owed, so the run must exit 2 and not report \
         the wizard as finished; stderr:\n{stderr}\nterminal transcript:\n{transcript}"
    );
    // And it says so, in the sentence this interaction is about.
    assert!(
        stderr.contains("NOT declared"),
        "the wizard that declined must say the step is unfinished; stderr:\n{stderr}"
    );
    // The decline recorded nothing: state on disk is what `status` and
    // `doctor` read on later runs, so an unwritten record is the difference
    // between "declined" and "done".
    assert!(
        !sandbox
            .state_dir()
            .join(chat_stasher::keydecl::KEY_DECLARATIONS_FILE)
            .exists(),
        "a declined prompt must record nothing at all; state dir:\n{}",
        sandbox.state_dir().display()
    );
    // The prompt really was the wizard's declaration question, not the
    // non-interactive refusal that never asks one.
    assert!(
        transcript.contains("The next answer is a declaration"),
        "the run took the interactive path, which explains the declaration before asking for \
         it; terminal transcript:\n{transcript}"
    );
}

/// W294: with several keys in play, an interactive "no" is still a "no".
///
/// The wizard asks once per archive copy, so a machine with a destination poses
/// two questions. Declining the destination's must leave *that* copy undeclared
/// while the local copy — answered with the sentence in the same run — is
/// recorded. A single-scope implementation could pass the decline test by
/// refusing everything or nothing; this pins the per-copy behaviour, and pins it
/// on the destination key, which is the one the run itself creates and so the
/// one a "the copy is old, of course it is declared" shortcut would get wrong.
#[cfg(unix)]
#[test]
fn an_interactive_no_on_one_of_several_keys_leaves_that_key_undeclared() {
    let sandbox = Sandbox::new();
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    // Both keys are on the disk before the interactive run: a key this run
    // creates is named and left owed, never prompted for, so the prompts — and
    // the per-key decline this test is about — are only reachable when the keys
    // already exist. The bootstrap names the destination, so it creates the
    // destination's own key as well as the local one.
    sandbox.bootstrap_keys(&["--destination", "backup"]);
    let stage = sandbox.stage();
    let mut wizard = InteractiveWizard::run(
        &sandbox,
        &[
            "setup",
            "--stage",
            stage.to_str().expect("a utf-8 stage path"),
            "--destination",
            "backup",
        ],
    );

    // The local key's prompt: answer with the sentence, so that copy *is*
    // declared and the test is about a decline for one key rather than about
    // declines in general.
    wizard.expect_within(
        "to declare you have your own copy",
        "the masterkey declaration prompt",
    );
    wizard.answer("I saved it elsewhere");

    // The destination's key already exists, so it gets its own prompt naming
    // that file. Decline it — the declaration must not be recorded for that
    // copy.
    wizard.expect_within(
        "destination key for `backup`",
        "the destination key declaration prompt",
    );
    wizard.answer("no");

    wizard.expect_within("Install the scheduler now?", "the scheduler prompt");
    wizard.answer("");

    let (status, stderr, transcript) = wizard.finish();

    assert_eq!(
        status.code(),
        Some(2),
        "the declined destination key leaves its step owed, so the run must exit 2; \
         stderr:\n{stderr}\nterminal transcript:\n{transcript}"
    );
    assert!(
        stderr.contains("NOT declared"),
        "the wizard must say the declined step is unfinished; stderr:\n{stderr}"
    );

    // The record is what `status`/`doctor` read, so "declined" has to mean
    // nothing was written for that scope — while the answered scope is there.
    let recorded = fs::read_to_string(
        sandbox
            .state_dir()
            .join(chat_stasher::keydecl::KEY_DECLARATIONS_FILE),
    )
    .expect("the local declaration, which was answered, must be recorded");
    assert!(
        recorded.contains(chat_stasher::keydecl::LOCAL_SCOPE),
        "the answered local declaration must be recorded: {recorded}"
    );
    assert!(
        !recorded.contains(&chat_stasher::keydecl::destination_scope("backup")),
        "a declined prompt must record nothing for its scope: {recorded}"
    );
}

/// W294: an interactive run never declares a key it just created.
///
/// The rule is the same one the headless path follows — a key created moments
/// ago is one nobody has had the chance to copy, so a "yes" recorded for it
/// would be a declaration about a backup that cannot exist. On a fresh machine
/// the wizard therefore does *not* print the declaration prompt for the local
/// key it mints during the run; it names the file, says the run cannot record
/// the declaration for it, and leaves the step owed for a later run. This is the
/// interactive half of the fix the headless suite pins from the other side.
#[cfg(unix)]
#[test]
fn an_interactive_run_never_declares_the_key_it_just_created() {
    let sandbox = Sandbox::new();
    let stage = sandbox.stage();
    let mut wizard = InteractiveWizard::run(
        &sandbox,
        &[
            "setup",
            "--stage",
            stage.to_str().expect("a utf-8 stage path"),
        ],
    );

    // The created-key notice, in the wizard's own words, and — the property
    // under test — no declaration prompt for that key.
    wizard.expect_within("was created by this run", "the created-key notice");
    assert!(
        !wizard
            .transcript
            .contains("to declare you have your own copy"),
        "an interactive run must not ask for the declaration of a key it created; \
         terminal transcript:\n{}",
        wizard.transcript
    );

    // The rest of the wizard still runs: the destination question and the
    // scheduler question, both answered with their defaults.
    wizard.expect_within(
        "Destination name (blank to skip",
        "the destination-name prompt",
    );
    wizard.answer("");
    wizard.expect_within("Install the scheduler now?", "the scheduler prompt");
    wizard.answer("");

    let (status, stderr, transcript) = wizard.finish();
    assert_eq!(
        status.code(),
        Some(2),
        "the local key was created by this run, so its declaration is still owed; \
         stderr:\n{stderr}\nterminal transcript:\n{transcript}"
    );
    // The record is what `status`/`doctor` read, so "not declared" has to mean
    // nothing was written, not merely that the transcript said so.
    assert!(
        !sandbox
            .state_dir()
            .join(chat_stasher::keydecl::KEY_DECLARATIONS_FILE)
            .exists(),
        "a run must record no declaration for a key it created itself"
    );
}

/// W294: the same rule applies to a destination key created by this run.
#[cfg(unix)]
#[test]
fn an_interactive_run_leaves_a_new_destination_key_undeclared() {
    let sandbox = Sandbox::new();
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    let stage = sandbox.stage();
    let mut wizard = InteractiveWizard::run(
        &sandbox,
        &[
            "setup",
            "--stage",
            stage.to_str().expect("a utf-8 stage path"),
            "--destination",
            "backup",
        ],
    );

    wizard.expect_within(
        "destination key for `backup` was created by this run",
        "the destination created-key notice",
    );
    assert!(
        !wizard
            .transcript
            .contains("to declare you have your own copy"),
        "the wizard must not ask for a declaration of a destination key it created; \
         terminal transcript:\n{}",
        wizard.transcript
    );
    wizard.expect_within("Install the scheduler now?", "the scheduler prompt");
    wizard.answer("");

    let (status, stderr, transcript) = wizard.finish();
    assert_eq!(
        status.code(),
        Some(2),
        "a newly created destination key remains owed; stderr:\n{stderr}\n\
         terminal transcript:\n{transcript}"
    );
    let destination_key = sandbox.data_root().join("masterkey-backup.json");
    assert!(
        destination_key.exists(),
        "dest-init must leave the key to copy"
    );
    assert!(
        !sandbox
            .state_dir()
            .join(chat_stasher::keydecl::KEY_DECLARATIONS_FILE)
            .exists(),
        "the run must not record a declaration for the key it just created"
    );
}

/// The pty number the kernel itself binds to `master_fd`, or `None` when the
/// kernel was not asked.
///
/// The harness reads a slave's path with `ptsname`, which on some libcs answers
/// out of storage the whole process shares ([`PTY_ALLOC`]). The ioctl here
/// writes into a buffer *this* call owns, so it is safe to call from any thread,
/// and it answers a question about the master this call holds: which slave has
/// the kernel given it. That is what makes it usable as the ground truth the test
/// below checks an allocation against.
#[cfg(target_os = "linux")]
fn master_slave_number(master_fd: RawFd) -> Option<u32> {
    // `_IOR('T', 0x30, unsigned int)` — the pty number of a master. glibc builds
    // its `ptsname` answer on exactly this call (`__ptsname_internal`), so the
    // number here is directly comparable with the path an allocation returns.
    let mut number: libc::c_uint = 0;
    let answered = unsafe { libc::ioctl(master_fd, libc::TIOCGPTN, &mut number) };
    (answered == 0).then_some(number as u32)
}

/// Darwin's counterpart to the Linux ioctl above.
///
/// `TIOCPTYGNAME` is the call Darwin's own `ptsname` is built on, and it answers
/// with the slave's *path* (`/dev/ttysNNN`) rather than its number, so the number
/// is read off the end of what it wrote. The constant is written out here because
/// `libc` declares these ioctls per platform and has no `TIOCPTYGNAME` for Apple:
/// `<sys/ttycom.h>` spells it `_IOC(IOC_OUT, 't', 83, 128)`, and a macOS run of
/// the test below had this ioctl and `ptsname` agree on all 16,384 allocations
/// (macOS 15.7).
#[cfg(target_os = "macos")]
fn master_slave_number(master_fd: RawFd) -> Option<u32> {
    const TIOCPTYGNAME: libc::c_ulong = 0x4080_7453;
    let mut name = [0u8; 128];
    let answered = unsafe { libc::ioctl(master_fd, TIOCPTYGNAME, name.as_mut_ptr()) };
    if answered != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(name.as_ptr().cast()) };
    trailing_number(name.to_str().ok()?)
}

/// No ioctl for "which slave does this master have" is wired up for this
/// platform, so the kernel is not asked and the answer is absent.
///
/// Absent, deliberately, rather than guessed: the test below counts these and
/// fails on the count, so an unwired platform is red with the reason rather than
/// green on a machine where the question was never put. macOS and Linux — the
/// platforms CI runs — are both wired up.
///
/// `unix` belongs in the guard because the signature is `RawFd`, which is
/// `std::os::fd` and exists only there. Off Unix there is no function at all
/// rather than a `None` one: nothing in this file is compiled there, since the
/// mechanism it drives is `posix_openpt` (the header says why). Without it the
/// arm was still built on Windows — `not(linux, macos)` is true there — and the
/// `windows-latest` clippy cell failed on exactly that name.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn master_slave_number(_master_fd: RawFd) -> Option<u32> {
    None
}

/// The number a slave path ends in: `/dev/pts/7` and `/dev/ttys007` are both 7.
///
/// Compared as numbers, not as strings: the two platforms spell the path
/// differently, and on one of them the ground truth above spells it differently
/// again. The number is the part the kernel is answering about, and it is the
/// part this suite needs — the path is only ever handed to `open`.
#[cfg(unix)]
fn trailing_number(path: &str) -> Option<u32> {
    let digits: String = path
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

/// W300: every terminal this suite allocates is the one the kernel bound to the
/// master the suite reads, however many tests are allocating at once.
///
/// This is the property the flake broke. `ptsname` answers out of one buffer for
/// the whole process on glibc ([`PTY_ALLOC`]), so with the wizard tests starting
/// together two of them could be handed the same slave path. The wizard would
/// then run on the other test's terminal: that test's transcript would carry a
/// stranger's output, and the test whose own master had no slave at all would
/// block in `read` until its deadline and report what the flake reported —
/// nothing. The check here asks the *kernel* which slave belongs to the master
/// just allocated and compares it with the path the allocation returned, so a
/// crossing is caught on the allocation that made it, by name, instead of
/// arriving as an empty transcript two minutes later.
///
/// The sample is large on purpose. A crossing needs two threads inside the few
/// hundred nanoseconds between `ptsname` returning and its result being copied
/// out, so it is rare per allocation — one in roughly two thousand on glibc 2.36
/// — and a single allocation per thread would prove almost nothing. 16 threads
/// × 1024 allocations put that rate at roughly eight crossings per run of this
/// test, which is what makes the *unfixed* harness fail here essentially every
/// time it is run rather than occasionally. The threads exist for the same
/// reason the runner's own schedule matters: the hazard is concurrency, so the
/// test has to supply it.
///
/// Platform statement, in this file's usual form: Darwin's `ptsname` keeps its
/// buffer per thread, so a crossing cannot happen there and this test cannot go
/// red on macOS whatever the harness does. That is not a reason to skip it — the
/// assertion still holds on every allocation on both platforms, and macOS still
/// checks that the path the suite hands a wizard is the path the kernel gave its
/// master. It is the reason the flake this pins was seen exactly once, on the
/// ubuntu cell, and never locally.
#[cfg(unix)]
#[test]
fn every_allocated_terminal_is_the_one_the_kernel_gave_its_master() {
    const THREADS: usize = 16;
    const PER_THREAD: usize = 1024;

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut workers = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            // Released together: an allocation that started while no other
            // thread was in `ptsname` cannot cross, so the threads have to be
            // in the window at the same time for this to be a real check.
            barrier.wait();
            let mut checked = 0usize;
            let mut crossed_count = 0usize;
            let mut unanswerable = 0usize;
            let mut crossed: Vec<String> = Vec::new();
            for _ in 0..PER_THREAD {
                let (master, _master_write, path) = allocate_pty();
                match master_slave_number(master.as_raw_fd()) {
                    Some(number) if Some(number) == trailing_number(&path) => checked += 1,
                    Some(number) => {
                        crossed_count += 1;
                        // Only the first few are quoted; the count above is what
                        // the assertions use, so a bad run cannot be shortened
                        // by this cap.
                        if crossed.len() < 8 {
                            crossed.push(format!(
                                "the kernel gave this master pty {number}, the allocation was \
                                 handed `{path}`"
                            ));
                        }
                    }
                    None => unanswerable += 1,
                }
                // Closed here: the check above is about the master this call
                // holds, so nothing has to stay open for it, and the machine's
                // pty pool is not held for the length of the run.
                drop(master);
            }
            (checked, crossed_count, unanswerable, crossed)
        }));
    }

    let mut checked = 0usize;
    let mut crossed_count = 0usize;
    let mut unanswerable = 0usize;
    let mut crossed: Vec<String> = Vec::new();
    for worker in workers {
        let (thread_checked, thread_crossed_count, thread_unanswerable, thread_crossed) =
            worker.join().expect("join an allocating thread");
        checked += thread_checked;
        crossed_count += thread_crossed_count;
        unanswerable += thread_unanswerable;
        crossed.extend(thread_crossed);
    }

    assert_eq!(
        unanswerable, 0,
        "the kernel did not say which slave belongs to {unanswerable} of the allocated masters, \
         so this test has no verdict on those allocations — an absence of crossings among the \
         rest is not evidence about them"
    );
    assert_eq!(
        checked + crossed_count,
        THREADS * PER_THREAD,
        "every allocation must have been made and checked; {checked} were checked and \
         {crossed_count} crossed out of {}",
        THREADS * PER_THREAD
    );
    assert!(
        crossed.is_empty(),
        "an allocation was handed a slave path that is not the one the kernel bound to its \
         master: the wizard would run on another test's terminal, or on one nothing reads, and \
         the test that owns it would block until its deadline with an empty transcript (W300): \
         {crossed:#?}"
    );
}

/// W348, the mechanism: a terminal this suite still holds keeps what a child
/// wrote before it exited, so a read that arrives after the child is gone still
/// finds it.
///
/// Read directly off the master, with no reader thread in between, so a failure
/// names the property rather than the harness. The child has been reaped and
/// the machine has been given a moment to behave like a loaded runner before
/// the read; without [`ParentTerminal`]'s descriptor the tty is torn down on the
/// child's exit and the kernel discards the unread buffer, and the read comes
/// back empty every time rather than occasionally — which is what makes this a
/// test of the mechanism and not of a race.
#[cfg(unix)]
#[test]
fn a_kept_terminal_still_holds_output_written_before_the_child_exited() {
    let (master_read, _master_write, slave_path) = allocate_pty();
    let parent_terminal = ParentTerminal::hold(&slave_path);
    let mut child = parent_terminal.spawn(sentinel_child(LATE_READER_SENTINEL));

    let status = child.wait().expect("wait for the sentinel child");
    assert!(
        status.success(),
        "the sentinel child must exit cleanly: {status:?}"
    );
    // Let the tty settle, the way a loaded runner delays the reader.
    std::thread::sleep(Duration::from_millis(100));

    let mut reader = master_read;
    let mut buffer = [0u8; 4096];
    let read = reader.read(&mut buffer).expect("read the kept terminal");
    let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
    assert!(
        text.contains(LATE_READER_SENTINEL),
        "the kept terminal lost the child's output, which is the empty transcript this harness \
         must never produce: {text:?}"
    );
    // Released only after the read, which is the order [`ParentTerminal`]
    // documents and the order `finish` follows: closing first would throw the
    // bytes away with the terminal.
    drop(parent_terminal);
}

/// W348, at the harness: a run whose reader arrives after the child exited still
/// has the child's output in its transcript.
///
/// This is the empty transcript itself, and it is asserted where it was
/// observed — on the string [`InteractiveWizard::finish`] returns, which is what
/// every wizard assertion in this file waits on. The reader is started *after*
/// the child has written and exited, which is the window a loaded runner opens
/// around the start of any run, and it is made deliberate here so the test
/// exercises that window every time instead of hoping to land in it.
///
/// Nothing about [`ParentTerminal`] is faked: the descriptor is held by the same
/// type every run holds, and the reader is the same thread [`spawn_terminal_reader`]
/// starts. Both halves have to be right for this to pass — the descriptor has to
/// be held for the tty to outlive the child, and `finish` has to drain before it
/// lets go, since closing the last slave is what ends the stream and takes the
/// unread buffer with it. Fix only the first and the drain finds a closed
/// terminal; fix only the second and the tty dies with the child. On the harness
/// as it was before W348 — no held descriptor at all — the transcript here is
/// empty and this assertion fails.
#[cfg(unix)]
#[test]
fn a_reader_that_starts_after_the_child_exited_still_drains_the_transcript() {
    let (master_read, master_write, slave_path) = allocate_pty();
    let parent_terminal = ParentTerminal::hold(&slave_path);
    let mut child = parent_terminal.spawn(sentinel_child(LATE_READER_SENTINEL));

    // Reaped with nothing reading the terminal: the writer has finished and the
    // reader has not started, which is the whole of the window.
    let status = child.wait().expect("wait for the sentinel child");
    assert!(
        status.success(),
        "the sentinel child must exit cleanly: {status:?}"
    );

    let terminal = spawn_terminal_reader(master_read);
    let wizard = InteractiveWizard {
        child,
        master_write,
        parent_terminal,
        transcript: String::new(),
        terminal,
    };
    let (status, _stderr, transcript) = wizard.finish();
    assert!(status.success(), "the run finished cleanly: {status:?}");
    assert!(
        transcript.contains(LATE_READER_SENTINEL),
        "a reader that arrived after the child exited must still be given what the child wrote; \
         an empty transcript here is the failure W300 and W348 both reported, and every wizard \
         assertion in this file is downstream of it not happening; transcript:\n{transcript}"
    );
}
