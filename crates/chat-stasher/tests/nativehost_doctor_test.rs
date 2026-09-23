//! `doctor`'s D8 check — the browser registration and the configured stage.
//!
//! `doctor` is a read-only probe, so the interesting assertions are about
//! *distinctions* rather than about counts: "no manifest here" is not "the
//! manifest is broken", "there is no stage key" is not "the configured stage is
//! gone", and a registered path that happens to live under `target/` is a
//! warning rather than a failure — it works right up until somebody runs
//! `cargo clean`.
//!
//! Everything is driven through [`doctor::inspect_native_host`] against a
//! `tempfile` home, so no real browser directory is read and nothing is
//! written. The probe takes the discovery *root* that was derived from that
//! home ([`root_for`]) rather than resolving one itself: a probe that resolved
//! its own root answered about wherever the process pointed, so on
//! `windows-latest` all nine tests in this file drove one shared
//! `%LOCALAPPDATA%` manifest file and failed one another. `the_probe_reads_only_under_the_root_it_is_given`
//! pins that.

use chat_stasher::config::{Config, NativeHostConfig};
use chat_stasher::doctor::{
    inspect_native_host, HostManifestState, NativeHostCheck, StageConfigCheck,
};
use chat_stasher::nativehost;
use std::fs;
use std::path::{Path, PathBuf};

/// The discovery root this build derives from `home` on this platform.
///
/// `default_root` is a pure function of `(platform, home)`, so deriving it here
/// is the same answer `doctor::run()` reaches on macOS and Linux — and on
/// Windows it is the answer for *this* home rather than the machine's
/// `%LOCALAPPDATA%`. That difference is the whole point: the root of a test
/// fixture must be inside that fixture's `tempfile` directory, or the tests in
/// this binary share one manifest file and assert against each other's writes.
fn root_for(home: &Path) -> PathBuf {
    nativehost::default_root(nativehost::Platform::current(), home)
}

/// D8 about `home`, the way a test means it: the root is derived, and the
/// platform's real layout under it is what gets exercised.
fn inspect(config: &Config, home: &Path) -> NativeHostCheck {
    inspect_native_host(config, &root_for(home))
}

/// The manifest path this build would use for Chrome under `root`.
fn chrome_manifest(root: &Path) -> PathBuf {
    nativehost::target(
        nativehost::Platform::current(),
        root,
        nativehost::Browser::Chrome,
        nativehost::HOST_NAME,
    )
    .expect("this build knows a Chrome path")
    .manifest
}

fn write_manifest(root: &Path, host_path: &Path) -> PathBuf {
    let manifest = chrome_manifest(root);
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(
        &manifest,
        serde_json::to_string_pretty(&serde_json::json!({
            "name": nativehost::HOST_NAME,
            "description": nativehost::HOST_DESCRIPTION,
            "path": host_path,
            "type": "stdio",
            "allowed_origins": [format!("chrome-extension://{}/", nativehost::CHROME_EXTENSION_ID)],
        }))
        .unwrap(),
    )
    .unwrap();
    manifest
}

/// Make `path` look like a binary the browser would agree to start: a file with
/// an execute bit where that idea exists.
fn plant_binary(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn chrome_state(check: &chat_stasher::doctor::NativeHostCheck) -> HostManifestState {
    check
        .manifests
        .iter()
        .find(|entry| entry.browser == "chrome")
        .expect("chrome is always checked")
        .state
        .clone()
}

fn config_with_stage(stage: Option<&Path>) -> Config {
    Config {
        native_host: Some(NativeHostConfig {
            stage: stage.map(|path| path.to_string_lossy().into_owned()),
            // `destination` only matters to `open_dashboard`; this fixture is
            // about the install manifests, so it stays unset.
            destination: None,
        }),
        ..Config::default()
    }
}

#[test]
fn an_unregistered_machine_says_so_without_calling_it_broken() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect(&Config::default(), &home);
    assert_eq!(check.manifests.len(), nativehost::Browser::ALL.len());
    for entry in &check.manifests {
        assert_eq!(
            entry.state,
            HostManifestState::NotRegistered,
            "{}: {:?}",
            entry.browser,
            entry.state
        );
    }
    assert_eq!(check.stage, StageConfigCheck::NotConfigured);
}

#[test]
fn a_registered_host_with_a_live_binary_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let binary = dir.path().join("bin").join("chat-stasher");
    plant_binary(&binary);
    write_manifest(&root_for(&home), &binary);

    let check = inspect(&Config::default(), &home);
    assert_eq!(chrome_state(&check), HostManifestState::Ok { path: binary });
}

#[test]
fn a_registered_host_whose_binary_is_gone_is_reported_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let gone = dir.path().join("bin").join("chat-stasher");
    write_manifest(&root_for(&home), &gone);

    let check = inspect(&Config::default(), &home);
    assert_eq!(
        chrome_state(&check),
        HostManifestState::PathMissing { path: gone }
    );
}

#[test]
fn a_binary_under_target_is_flagged_as_a_build_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let binary = dir
        .path()
        .join("repo")
        .join("target")
        .join("debug")
        .join("chat-stasher");
    plant_binary(&binary);
    write_manifest(&root_for(&home), &binary);

    let check = inspect(&Config::default(), &home);
    assert_eq!(
        chrome_state(&check),
        HostManifestState::BuildArtifact { path: binary },
        "a `target/debug` path is exactly the registration that dies on `cargo clean`"
    );
}

/// The execute bit is a Unix idea. Both arms assert, because a one-sided `cfg`
/// guard here would leave the platform it compiles out completely unchecked.
#[test]
fn a_non_executable_binary_is_reported_only_where_executability_exists() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let binary = dir.path().join("bin").join("chat-stasher");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
    }
    write_manifest(&root_for(&home), &binary);

    let check = inspect(&Config::default(), &home);
    let state = chrome_state(&check);
    if cfg!(unix) {
        assert_eq!(
            state,
            HostManifestState::PathNotExecutable { path: binary },
            "a file without an execute bit cannot be started by the browser"
        );
    } else {
        assert_eq!(
            state,
            HostManifestState::Ok { path: binary },
            "this platform has no execute bit, so the question does not exist here"
        );
    }
}

#[test]
fn a_manifest_that_is_not_json_is_reported_as_invalid_not_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let manifest = chrome_manifest(&root_for(&home));
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(&manifest, "this is not json").unwrap();

    let check = inspect(&Config::default(), &home);
    match chrome_state(&check) {
        HostManifestState::Invalid { error } => assert!(!error.is_empty()),
        other => panic!("a present-but-broken manifest must not read as absent: {other:?}"),
    }
}

#[test]
fn a_manifest_without_a_path_field_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let manifest = chrome_manifest(&root_for(&home));
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(
        &manifest,
        r#"{"name":"com.chat_stasher.host","type":"stdio"}"#,
    )
    .unwrap();

    let check = inspect(&Config::default(), &home);
    match chrome_state(&check) {
        HostManifestState::Invalid { error } => assert!(error.contains("path"), "{error}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn the_three_stage_findings_stay_three_findings() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // Absent key.
    assert_eq!(
        inspect(&Config::default(), &home).stage,
        StageConfigCheck::NotConfigured
    );

    // Configured and present.
    let present = dir.path().join("stage");
    fs::create_dir_all(&present).unwrap();
    assert_eq!(
        inspect(&config_with_stage(Some(&present)), &home).stage,
        StageConfigCheck::Present {
            path: present.clone()
        }
    );

    // Configured, and nothing is there.
    let absent = dir.path().join("never-created");
    assert_eq!(
        inspect(&config_with_stage(Some(&absent)), &home).stage,
        StageConfigCheck::Missing {
            path: absent.clone()
        }
    );

    // Configured, and the path is something else entirely.
    let file = dir.path().join("not-a-dir");
    fs::write(&file, "x").unwrap();
    assert_eq!(
        inspect(&config_with_stage(Some(&file)), &home).stage,
        StageConfigCheck::NotADirectory { path: file }
    );

    // And the check created nothing along the way.
    assert!(!absent.exists());
}

#[test]
fn the_json_shape_keeps_absence_and_unknown_apart() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect(&Config::default(), &home);
    let json = chat_stasher::doctor::native_host_json(&check);
    assert_eq!(json["checked"], true);
    assert_eq!(json["registered"], 0);
    assert_eq!(json["stage"]["kind"], "not_configured");
    for manifest in json["manifests"].as_array().unwrap() {
        assert_eq!(manifest["kind"], "not_registered");
        assert!(
            manifest.get("path").is_none(),
            "an unregistered browser has no path to report: {manifest}"
        );
    }
}

/// The probe answers about the root it is handed, and about nothing else.
///
/// This is the regression test for the Windows nightly. The probe used to
/// resolve its own root from the process environment, so on `windows-latest`
/// every test in this binary read and wrote the *same*
/// `%LOCALAPPDATA%\chat-stasher\...\<host>.json` and asserted against whichever
/// test wrote last — which is why four of them failed together while their
/// assertions, in isolation, were all correct.
///
/// A registration that is present under another root is not this root's
/// registration: the answer for the root being probed is `NotRegistered`, and
/// no count moves.
#[test]
fn the_probe_reads_only_under_the_root_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let populated = dir.path().join("populated");
    let probed = dir.path().join("probed");
    fs::create_dir_all(&probed).unwrap();

    // A live-looking registration, planted under a root this call does not name.
    let binary = dir.path().join("bin").join("chat-stasher");
    plant_binary(&binary);
    let elsewhere = write_manifest(&populated, &binary);
    assert!(elsewhere.is_file(), "fixture: the decoy must exist");

    let check = inspect_native_host(&Config::default(), &probed);
    for entry in &check.manifests {
        assert_eq!(
            entry.state,
            HostManifestState::NotRegistered,
            "{}: the probe reported a registration under a root it was not asked \
             about ({:?})",
            entry.browser,
            entry.state
        );
    }
    let json = chat_stasher::doctor::native_host_json(&check);
    assert_eq!(
        json["registered"], 0,
        "a manifest outside the probed root must not be counted: {json}"
    );
}

/// Two homes never name one browser registration.
///
/// The other half of the regression above, and the half the four reds of
/// 2026-09-14 onward actually came from: the root [`default_root`] answers with
/// has to be a fact about the home it was handed. On `windows-latest` it was
/// not — the root came from `%LOCALAPPDATA%` in the process environment instead
/// — so the two homes below both answered with one directory, every test in
/// this binary planted its manifest in the same file, and whichever test ran
/// last decided what the others read.
///
/// `%LOCALAPPDATA%` is pointed at a third directory for the duration of the
/// reads, and that is the whole point of the test. `nativehost.rs`'s
/// `the_root_is_derived_from_the_home_it_is_given` pins the same shape, but it
/// cannot go red on macOS or Linux: the variable is unset there, so the code
/// that was red on Windows answers exactly as the fixed code does. This one is
/// red on every platform against the code that was red on one — which is what
/// makes it usable by the grids that run every day rather than by the nightly
/// that went unread for five nights.
///
/// The variable is restored before any assertion runs, so a failing assertion
/// here cannot leave a process-global set for the rest of this binary.
#[test]
fn two_homes_never_share_a_browser_registration() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    // A profile can redirect the `LocalAppData` known folder — `machine_root`
    // exists to honour that for callers that mean *this machine*. A caller that
    // named a home is not such a caller, and that is the distinction under test.
    let decoy = dir.path().join("redirected-local-appdata");

    fn manifest_for(platform: nativehost::Platform, root: &Path) -> PathBuf {
        nativehost::target(
            platform,
            root,
            nativehost::Browser::Chrome,
            nativehost::HOST_NAME,
        )
        .expect("this build knows a Chrome path")
        .manifest
    }

    let previous = std::env::var_os("LOCALAPPDATA");
    std::env::set_var("LOCALAPPDATA", &decoy);
    let observed: Vec<(nativehost::Platform, PathBuf, PathBuf, PathBuf, PathBuf)> = [
        nativehost::Platform::Macos,
        nativehost::Platform::Linux,
        nativehost::Platform::Windows,
    ]
    .into_iter()
    .map(|platform| {
        let left = nativehost::default_root(platform, &first);
        let right = nativehost::default_root(platform, &second);
        let left_manifest = manifest_for(platform, &left);
        let right_manifest = manifest_for(platform, &right);
        (platform, left, right, left_manifest, right_manifest)
    })
    .collect();
    match &previous {
        Some(value) => std::env::set_var("LOCALAPPDATA", value),
        None => std::env::remove_var("LOCALAPPDATA"),
    }

    for (platform, left, right, left_manifest, right_manifest) in observed {
        assert_ne!(
            left,
            right,
            "{}: two homes, one discovery root",
            platform.id()
        );
        assert_ne!(
            left,
            decoy,
            "{}: the root came from `%LOCALAPPDATA%` rather than from the home this \
             call named",
            platform.id()
        );
        assert_ne!(
            left_manifest,
            right_manifest,
            "{}: two homes, one browser registration — a fixture asking about one home \
             would be answered about another",
            platform.id()
        );
    }
}
