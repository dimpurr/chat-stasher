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
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// Generous, because the wizard's local first save does real archive work
/// between prompts; what matters is that a wait never hangs the suite — every
/// wait ends in either a found prompt or a panic that prints the transcript.
#[cfg(unix)]
const WAIT: Duration = Duration::from_secs(120);

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
    transcript: String,
    terminal: mpsc::Receiver<Terminal>,
}

#[cfg(unix)]
impl InteractiveWizard {
    /// Allocate a pseudo-terminal and run the binary on the slave side of it.
    ///
    /// Safety: the `libc` calls here are the pty allocation dance on a fresh
    /// file descriptor this test owns — `posix_openpt` to allocate,
    /// `grantpt`/`unlockpt` to make the slave openable, `ptsname` for its path
    /// — each checked for failure the same way `std::fs` would report it. The
    /// master fd is wrapped in a `File` the moment `ptsname` has read its
    /// path, so nothing leaks. `ptsname` hands back a pointer into a libc
    /// buffer; it is copied out into an owned `String` inside this call, and
    /// this suite is the only thing in the process touching a pty.
    fn run(sandbox: &Sandbox, args: &[&str]) -> Self {
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

        // Two opens of the slave path — read end for stdin, write end for
        // stdout — because `Stdio` takes an owned file per stream. Both close
        // in the parent when `spawn` has duplicated them into the child, so
        // the pty ends when the child does and the reader below sees it.
        let slave_stdin = fs::File::open(&slave_path)
            .unwrap_or_else(|error| panic!("open the pty slave for stdin: {error}"));
        let slave_stdout = fs::OpenOptions::new()
            .write(true)
            .open(&slave_path)
            .unwrap_or_else(|error| panic!("open the pty slave for stdout: {error}"));

        let mut command = sandbox.command(args);
        command
            .stdin(Stdio::from(slave_stdin))
            .stdout(Stdio::from(slave_stdout))
            .stderr(Stdio::piped());
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("run the wizard on the terminal: {error}"));

        let (sender, terminal) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = master_read;
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        // End of the slave side: the child exited (or the
                        // terminal broke). Both mean no more output; the
                        // driver's deadline, transcript, and exit status are
                        // what report the difference. A send that finds
                        // nobody listening is the driver being gone — not a
                        // failure worth reporting — so it is dropped rather
                        // than `let _`-ed.
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

        InteractiveWizard {
            child,
            master_write,
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
        // The child is gone, so its stderr is finite; drain it whole.
        let mut stderr = String::new();
        let mut pipe = self.child.stderr.take().expect("stderr was piped");
        pipe.read_to_string(&mut stderr)
            .expect("read the wizard's stderr");
        (status, stderr, self.transcript)
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

    // The destination's key is created by the destination step in this very run;
    // its prompt names that file. Decline it — the declaration must not be
    // recorded for that copy.
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
