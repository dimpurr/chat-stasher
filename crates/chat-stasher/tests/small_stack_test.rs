//! The CLI must not depend on the platform's main-thread stack size.
//!
//! Windows gives the main thread 1 MiB where Unix gives 8, and on 2026-08-28
//! that difference took the Windows CI job down: `#[derive(Parser)]` expands
//! into a builder chain over every subcommand and argument, which in a debug
//! build measured ~944 KiB on its own. The process died with
//! STATUS_STACK_OVERFLOW inside `Cli::parse`, before `main` printed anything —
//! so the *reported* failure was a missing diagnostic line in an unrelated
//! test, and the real cause stayed hidden for days.
//!
//! `main` now does its work on a thread whose stack it sizes itself (16 MiB).
//! The two arms pin that from opposite directions:
//!
//!   * **Unix** lowers the stack with `ulimit -s` and runs the real binary
//!     under a limit well below what Windows gives its main thread — argument
//!     parsing must still survive. The guard is `cfg(unix)` rather than
//!     `cfg(target_os)` because `ulimit -s` is POSIX and behaves the same on
//!     Linux and macOS (an earlier guard in this repo used a BSD-only `stat`
//!     flag and silently never fired on Linux). Windows has no `ulimit`, so
//!     the stack-lowering dimension cannot be reproduced there through this
//!     API.
//!   * **Windows** runs the un-lowered binary. That is not a weaker probe: the
//!     original defect overflowed the *ordinary* 1 MiB main thread, so on the
//!     platform that actually had the bug, a bare `--version` is the full
//!     regression test. Before the `main`-on-16-MiB-thread fix this arm fails
//!     with STATUS_STACK_OVERFLOW (exit 0xC00000FD) and no version output.

use std::process::Command;

/// Well under the 1 MiB Windows main thread, and far under the ~944 KiB the
/// unfixed debug binary needed. Not lower than this: the limit also caps the
/// spawning thread, and the OS needs room to create the worker thread at all.
const TINY_STACK_KIB: u32 = 256;

#[cfg(unix)]
#[test]
fn argument_parsing_survives_a_stack_smaller_than_windows_gives() {
    let bin = env!("CARGO_BIN_EXE_chat-stasher");
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("ulimit -s {TINY_STACK_KIB}; exec \"$0\" --version",))
        .arg(bin)
        .output()
        .expect("spawn sh");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Checked before the exit code so that a stack overflow reports as itself
    // rather than as a bare non-zero status.
    assert!(
        !stderr.contains("has overflowed its stack"),
        "the CLI must not need more stack than Windows gives its main thread; \
         stderr={stderr}"
    );
    assert!(
        output.status.success(),
        "`--version` under a {TINY_STACK_KIB} KiB stack must succeed; exit={:?}\n\
         stdout={stdout}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("chat-stasher"),
        "`--version` must still print the version; stdout={stdout}\nstderr={stderr}"
    );
}

#[cfg(windows)]
#[test]
fn argument_parsing_survives_the_windows_main_thread_stack() {
    let bin = env!("CARGO_BIN_EXE_chat-stasher");
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .expect("spawn binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The original defect died inside Cli::parse with STATUS_STACK_OVERFLOW —
    // a non-zero exit with nothing on stdout. Both conditions are asserted.
    assert!(
        output.status.success(),
        "`--version` must succeed on Windows' 1 MiB main thread; exit={:?}\n\
         stdout={stdout}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("chat-stasher"),
        "`--version` must still print the version; stdout={stdout}\nstderr={stderr}"
    );
}
