//! W289: a spawned `chat-stasher` must resolve its rustic metadata cache into
//! the sandbox it was handed, never into the machine's real user cache.
//!
//! `HOME` and `XDG_CACHE_HOME` are not enough to guarantee that. rustic
//! resolves its cache root with `dirs::cache_dir()` (rustic_core-0.12.0
//! `src/backend/cache.rs:261`), and on Windows that is the Known Folder API —
//! `%LOCALAPPDATA%`, which no environment variable redirects (`dirs-6.0.0`
//! `src/win.rs:10` → `known_folder_local_app_data`). The relocation therefore
//! travels through the product's own `rustic_cache_dir` knob, which
//! [`test_support::RUSTIC_CACHE_DIR_ENV`] sets, exactly as
//! `CONTRIBUTING.md` describes.
//!
//! The test asks the spawned binary itself where its cache is — `doctor --json`
//! reports the resolved root — so it goes red the moment a spawn stops pinning
//! the cache, on **every** platform, not only on Windows. The second test runs
//! the same command with the variable removed: without a pin the root really is
//! the platform default, which is what makes the first test's assertion
//! discriminate instead of passing for a reason unrelated to the fixture.
//!
//! Nothing is written into the real cache root by either test: `doctor` only
//! *measures* that directory, and with no repository inside the sandbox there
//! is nothing for it to open and index.

#[path = "../src/test_support.rs"]
mod test_support;

use std::fs;
use std::path::Path;
use std::process::Command;

/// Run `doctor --json` under a fully sandboxed HOME/XDG environment, with the
/// fixture's cache pin present or deliberately absent.
fn doctor_json(sandbox: &Path, pin_cache: bool) -> serde_json::Value {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W289 synthetic","harnesses":[]}"#,
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(["doctor", "--json"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry);
    if pin_cache {
        command.env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&sandbox),
        );
    } else {
        command.env_remove(test_support::RUSTIC_CACHE_DIR_ENV);
    }

    let output = command.output().expect("spawn chat-stasher doctor");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"))
}

/// The cache root the spawned binary resolved, as it reports it.
fn reported_root(v: &serde_json::Value) -> String {
    v["cache"]["root"]
        .as_str()
        .unwrap_or_else(|| panic!("doctor --json must report the cache root: {v}"))
        .to_string()
}

/// The point of the whole W289 effort: a spawned child's cache lands in the
/// sandbox the fixture pinned, not in the real user cache.
#[test]
fn a_spawned_binary_resolves_its_cache_into_the_sandbox() {
    let sandbox = tempfile::tempdir().unwrap();
    let v = doctor_json(sandbox.path(), true);
    let expected = test_support::rustic_cache_root(sandbox.path());
    assert_eq!(
        reported_root(&v),
        expected.display().to_string(),
        "a pinned cache must be the root the spawned binary resolves — the real \
         user cache must never appear here"
    );
}

/// The control: with the pin removed, `doctor` resolves the platform default
/// instead — `dirs::cache_dir()/rustic`, spelled the way the *running* platform
/// spells it, for the environment the child was given. If that were ever the
/// pinned root too, the test above would be proving nothing.
#[test]
fn without_the_pin_the_spawned_binary_falls_back_to_the_platform_default() {
    let sandbox = tempfile::tempdir().unwrap();
    let v = doctor_json(sandbox.path(), false);

    // The child's environment, not this process's: `HOME` and `XDG_CACHE_HOME`
    // are the sandbox's, and `%LOCALAPPDATA%` is inherited untouched, so the
    // Windows spelling of the default is the runner's real one. Resolving it
    // through the product's own helper keeps this from being a second spelling
    // that could drift from `dirs`.
    let home = sandbox.path().join("home");
    let xdg = sandbox.path().join("xdg-cache");
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from);
    let default_root = chat_stasher::scanner::user_cache_dirs_on(
        chat_stasher::scanner::current_platform(),
        &home,
        Some(&xdg),
        local.as_deref(),
    )
    .into_iter()
    .next()
    .expect("every supported platform names at least one cache root")
    .join("rustic");

    assert_eq!(
        reported_root(&v),
        default_root.display().to_string(),
        "unpinned, the resolved root must be the platform default — otherwise \
         `a_spawned_binary_resolves_its_cache_into_the_sandbox` proves nothing"
    );
    assert_ne!(
        default_root,
        test_support::rustic_cache_root(sandbox.path()),
        "the sandbox pin must differ from the platform default"
    );
}
