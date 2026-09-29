//! W244 — the reader guard must leave the process's panic hook exactly as it
//! found it.
//!
//! The hook is process-wide state: installing one changes what every thread in
//! the process does when it panics, most of which this crate does not own. The
//! guard's design rests on installing nothing — `catching_panic` runs the reader
//! call on a thread it spawns and joins, and the process's hook afterwards is
//! the one it had before. `crates/chat-stasher/src/reader_guard.rs` says why an
//! earlier revision installed a hook and why that was removed rather than
//! narrowed.
//!
//! This test installs a hook of its own to watch for that: a hook the guard had
//! replaced would never see the panic below.
//!
//! It lives in `tests/` rather than beside the code for one reason: cargo runs
//! every file here as its own process, so a hook this test installs cannot
//! reach another test. A `#[cfg(test)]` unit test shares its process with the
//! rest of the unit-test binary, where a wrapper left installed is visible to
//! every test running in parallel — and handing the hook back *exactly*, rather
//! than wrapped, is only possible here, where this test is the whole process.
//! Its predecessor took the hook and put back a closure that called the old one
//! instead of the `Box` it took, which is a second layer on a global that
//! outlives the test.
//!
//! **Keep this file to one test.** A second test here would share the process
//! and give back the isolation this file exists to have; another hook-touching
//! test belongs in a file of its own.

use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn the_guard_leaves_the_panic_hook_alone() {
    const MARKER: &str = "a panic the counting hook must see";
    static SEEN: AtomicUsize = AtomicUsize::new(0);

    // Park the hook the process has and put a counting one in its place. This
    // hook deliberately does not call through: nothing else runs in this
    // process, and not calling through is what leaves the parked `Box` intact
    // to be reinstalled at the end, instead of it being consumed by a wrapper
    // that would then have to stand in for it.
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        // Only this test's own panic is counted; the marker is what makes the
        // assertion below a statement about *this* panic rather than about any
        // panic at all.
        if info.payload().downcast_ref::<&str>() == Some(&MARKER) {
            SEEN.fetch_add(1, Ordering::SeqCst);
        }
    }));

    assert_eq!(
        chat_stasher::reader_guard::catching_panic("read", || Ok(1)).expect("a healthy read"),
        1
    );

    // The literal matters: `panic!` with a format argument carries a `String`
    // payload, and this hook identifies its own panic by the `&'static str` one
    // a bare literal produces.
    let panicker = std::thread::spawn(|| panic!("a panic the counting hook must see"));
    assert!(panicker.join().is_err(), "the other thread was to panic");
    assert!(
        SEEN.load(Ordering::SeqCst) >= 1,
        "the panic never reached the installed hook, so the guarded call replaced it"
    );

    // Hand the process back the hook it had, exactly. `set_hook` hands nothing
    // back, so the counting hook comes off with `take_hook()` — whose return
    // value is the hook itself, dropped here — and then the `Box` taken above
    // goes back in: the hook itself, not a closure that calls it.
    drop(panic::take_hook());
    panic::set_hook(previous);
}
