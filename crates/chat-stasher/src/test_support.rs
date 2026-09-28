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
