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
    inspect_native_host, native_host_step, HostManifestCheck, HostManifestState, NativeHostCheck,
    StageConfigCheck,
};
use chat_stasher::nativehost::{self, Support};
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

// ---------------------------------------------------------------------------
// The per-browser inventory (EXT-1): detected? registered? and what that is not
// ---------------------------------------------------------------------------

/// One probe of an empty temp home, per browser, with the tier beside it.
///
/// `detected` is the browser's *data directory*, which is a fact about the
/// browser and never about the extension. This test pins the three-valued shape
/// so it cannot be read as an install check: on Windows the four Chrome-family
/// browsers have a probe (their data directories are in Chromium's own
/// documentation) and everything else answers `unknown`.
#[test]
fn every_browser_reports_its_tier_and_whether_it_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect(&Config::default(), &home);
    assert_eq!(check.manifests.len(), nativehost::Browser::ALL.len());

    for entry in &check.manifests {
        let browser = nativehost::Browser::ALL
            .iter()
            .copied()
            .find(|browser| browser.id() == entry.browser)
            .expect("every row names a browser the enum has");

        assert_eq!(
            entry.support,
            browser.support(nativehost::Platform::current()),
            "{}: the tier a reader sees is the tier the matrix documents",
            entry.browser
        );

        let probed = if cfg!(target_os = "windows") {
            matches!(
                browser,
                nativehost::Browser::Chrome
                    | nativehost::Browser::ChromeBeta
                    | nativehost::Browser::ChromeCanary
                    | nativehost::Browser::Chromium
            )
        } else {
            true
        };
        if probed && entry.support.is_some() {
            // An empty home has no browser in it. `Some(false)` and not `None`:
            // we looked, and it is not there.
            assert_eq!(
                entry.detected,
                Some(false),
                "{}: an empty home detects nothing",
                entry.browser
            );
        } else {
            assert_eq!(
                entry.detected, None,
                "{}: no probe on this platform means unknown, which is not `false`",
                entry.browser
            );
        }

        // Nothing was planted, so nothing is registered — and each row still
        // names the path it looked at, so a reader can tell "wrong path" from
        // "right path, nothing there".
        assert_eq!(entry.state, HostManifestState::NotRegistered);
        assert!(
            entry.manifest.is_some(),
            "{}: a supported pair must name the path that was probed",
            entry.browser
        );
    }
}

/// A pair outside this build's path table is reported as such, and never as
/// "not registered".
///
/// Arc ships for macOS and Windows only, so on Linux there is nothing to look
/// at. Reporting that as `NotRegistered` would tell a Linux user their Arc
/// registration is missing — a finding about a browser that cannot exist there,
/// and one no user action could fix.
#[test]
fn a_pair_outside_the_path_table_is_not_reported_as_unregistered() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect(&Config::default(), &home);
    let arc = check
        .manifests
        .iter()
        .find(|entry| entry.browser == "arc")
        .expect("arc is always a row");

    // Both arms assert. A one-sided `cfg` guard would leave the platform it
    // compiles out with no coverage of the distinction this test is for.
    if cfg!(target_os = "linux") {
        assert_eq!(arc.state, HostManifestState::NoDiscoveryPath);
        assert_eq!(arc.support, None);
        assert!(
            arc.manifest.is_none(),
            "a pair with no discovery path has no path to report"
        );
    } else {
        assert_eq!(
            arc.support,
            Some(Support::Supported),
            "Arc is in D5's promised set where it ships"
        );
        assert_eq!(arc.state, HostManifestState::NotRegistered);
        assert!(arc.manifest.is_some());
    }

    // On every platform, the *set* of pairs outside the table is exactly the
    // set whose tier is `None` — the two are one fact, asked twice.
    for entry in &check.manifests {
        assert_eq!(
            entry.state == HostManifestState::NoDiscoveryPath,
            entry.support.is_none(),
            "{}: `no_discovery_path` and `no tier` must agree",
            entry.browser
        );
    }
}

/// The JSON separates the three answers, and carries no field that could be read
/// as "the extension is installed".
///
/// A data directory outlives an uninstall and is shared by every profile, so a
/// browser being `detected` says nothing about the extension. There is therefore
/// deliberately no `installed` field anywhere in this object — this pins that,
/// because an `installed` key added later would be read as one.
#[test]
fn the_json_never_offers_an_installed_field() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect(&Config::default(), &home);
    let json = chat_stasher::doctor::native_host_json(&check);

    for row in json["manifests"].as_array().unwrap() {
        assert!(
            row.get("installed").is_none(),
            "a browser's data directory must never be serialised as `installed`: {row}"
        );
        assert!(row.get("detected").is_some(), "{row}");
        assert!(row.get("registered").is_some(), "{row}");
        assert!(row.get("support").is_some(), "{row}");
    }

    // The three lists are disjoint by construction, and `unsupported` is the
    // only place a pair with no path appears.
    let supported: Vec<&str> = json["supported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let unverified: Vec<&str> = json["unverified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let unsupported: Vec<&str> = json["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();

    assert!(supported.contains(&"chrome"), "{json}");
    assert!(supported.contains(&"firefox"), "{json}");
    assert!(
        unverified.contains(&"chrome-canary") && unverified.contains(&"opera"),
        "D5's best-effort set must be the unverified list: {json}"
    );
    for id in &unsupported {
        assert!(
            !supported.contains(id) && !unverified.contains(id),
            "{id} is both unsupported and tiered: {json}"
        );
    }
    if cfg!(target_os = "linux") {
        assert_eq!(unsupported, vec!["arc"], "{json}");
    } else {
        assert!(unsupported.is_empty(), "{json}");
    }
}

/// `detected but not registered` is the actionable intersection, and it moves
/// when a manifest lands.
#[test]
fn detected_and_registered_are_separate_answers() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = root_for(&home);
    fs::create_dir_all(&root).unwrap();

    // Chrome's data directory is here; no manifest is.
    let chrome_probe = nativehost::target(
        nativehost::Platform::current(),
        &root,
        nativehost::Browser::Chrome,
        nativehost::HOST_NAME,
    )
    .expect("this build knows a Chrome path");
    if let Some(probe) = &chrome_probe.profile_root {
        fs::create_dir_all(probe).unwrap();
    }

    let before = chat_stasher::doctor::native_host_json(&inspect(&Config::default(), &home));
    let list = |value: &serde_json::Value, key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        list(&before, "detected").contains(&"chrome".to_string()),
        "a browser whose data directory exists is detected: {before}"
    );
    assert!(
        list(&before, "detected_not_registered").contains(&"chrome".to_string()),
        "{before}"
    );

    // Now register it. The same machine now has that browser *and* a host
    // registration, which is a different pair of answers.
    let binary = dir.path().join("bin").join("chat-stasher");
    plant_binary(&binary);
    write_manifest(&root, &binary);

    let after = chat_stasher::doctor::native_host_json(&inspect(&Config::default(), &home));
    assert!(
        !list(&after, "detected_not_registered").contains(&"chrome".to_string()),
        "registering must clear the actionable list, not the detected one: {after}"
    );
    assert!(
        list(&after, "detected").contains(&"chrome".to_string()),
        "the browser is still here after registering: {after}"
    );
    assert_eq!(after["registered"], 1, "{after}");
}

/// The step slug is three-valued, and each value is reachable.
///
/// Built by hand rather than through a probe: `nothing_to_look_at` needs a
/// platform with no path table at all, which none of the three has — so a test
/// that could only reach two of the three states would leave the third
/// unpinned.
#[test]
fn the_host_step_is_three_valued() {
    let row = |browser: &str, state: HostManifestState| HostManifestCheck {
        browser: browser.to_string(),
        support: Some(Support::Supported),
        detected: Some(true),
        manifest: Some(PathBuf::from("/nowhere/com.chat_stasher.host.json")),
        state,
    };
    let check = |manifests: Vec<HostManifestCheck>| NativeHostCheck {
        manifests,
        stage: StageConfigCheck::NotConfigured,
    };

    assert_eq!(
        native_host_step(&check(vec![row(
            "chrome",
            HostManifestState::Ok {
                path: PathBuf::from("/bin/chat-stasher")
            }
        )])),
        "registered"
    );
    assert_eq!(
        native_host_step(&check(vec![
            row("chrome", HostManifestState::NotRegistered),
            row(
                "edge",
                HostManifestState::PathMissing {
                    path: PathBuf::from("/gone")
                }
            ),
        ])),
        // A broken registration still counts as registered: the file is there
        // and the finding is about its *content*. `registered` is not a verdict
        // on health, and this pins that it is not read as one.
        "registered"
    );
    assert_eq!(
        native_host_step(&check(vec![row(
            "chrome",
            HostManifestState::NotRegistered
        )])),
        "none_registered"
    );
    assert_eq!(
        native_host_step(&check(vec![HostManifestCheck {
            browser: "arc".to_string(),
            support: None,
            detected: None,
            manifest: None,
            state: HostManifestState::NoDiscoveryPath,
        }])),
        "nothing_to_look_at"
    );
}

/// Every row carries the four facts, and two different states never render the
/// same line.
///
/// This exists because of a *coverage* loss rather than a defect. D8's rows used
/// to be twelve `eprintln!` literals, which `scripts/output-inventory.py` lists;
/// sharing them with the setup wizard (EXT-1) turned them into `format!` calls,
/// and that script covers `println!`/`eprintln!` first literals but explicitly
/// not `format!`. The strings are still printed, but nothing enumerates them any
/// more — so the property they need is asserted here instead, and it is the
/// property that actually matters: a reader of one row can recover which browser,
/// which tier, whether the browser is present, and what is at the path.
#[test]
fn every_row_carries_all_four_facts_and_no_two_states_look_alike() {
    let path = PathBuf::from("/somewhere/com.chat_stasher.host.json");
    let binary = PathBuf::from("/bin/chat-stasher");

    let states: Vec<HostManifestState> = vec![
        HostManifestState::NoDiscoveryPath,
        HostManifestState::NotRegistered,
        HostManifestState::Unreadable {
            error: "permission denied".to_string(),
        },
        HostManifestState::Invalid {
            error: "expected value at line 1".to_string(),
        },
        HostManifestState::PathMissing { path: path.clone() },
        HostManifestState::PathNotExecutable { path: path.clone() },
        HostManifestState::BuildArtifact { path: path.clone() },
        HostManifestState::Ok {
            path: binary.clone(),
        },
    ];

    let mut rendered: Vec<(HostManifestState, String)> = Vec::new();
    for state in &states {
        let no_path = *state == HostManifestState::NoDiscoveryPath;
        let entry = HostManifestCheck {
            browser: "chrome".to_string(),
            support: if no_path {
                None
            } else {
                Some(Support::Supported)
            },
            detected: if no_path { None } else { Some(true) },
            manifest: if no_path { None } else { Some(path.clone()) },
            state: state.clone(),
        };
        let line = chat_stasher::doctor::host_row_line(&entry);

        assert!(
            line.contains("chrome"),
            "a row must name its browser: {line}"
        );
        assert!(
            line.contains(if no_path { "-" } else { "supported" }),
            "a row must name its tier, and `-` where there is none: {line}"
        );
        assert!(
            line.contains(if no_path { "unknown" } else { "yes" }),
            "a row must answer `detected`, in three words and not two: {line}"
        );
        if no_path {
            assert!(
                line.contains("no discovery path"),
                "a pair outside the path table must say so rather than read as absent: {line}"
            );
        } else {
            // Which path a row names is part of the finding: the states that
            // describe the *manifest* name where it was read, and the states
            // that describe the binary it points at name that binary. A row that
            // named the wrong one would send a reader to the wrong file.
            let named = match state {
                HostManifestState::NoDiscoveryPath => unreachable!("handled above"),
                HostManifestState::NotRegistered
                | HostManifestState::Unreadable { .. }
                | HostManifestState::Invalid { .. } => path.clone(),
                HostManifestState::PathMissing { path }
                | HostManifestState::PathNotExecutable { path }
                | HostManifestState::BuildArtifact { path }
                | HostManifestState::Ok { path } => path.clone(),
            };
            assert!(
                line.contains(&named.display().to_string()),
                "{state:?} must name {}: {line}",
                named.display()
            );
        }
        rendered.push((state.clone(), line));
    }

    // Eight states, eight rows: a collision would mean one finding is invisible
    // behind another. The two shared suffixes (`registered, but …`) are the pair
    // most at risk of reading the same.
    for (i, (state_a, line_a)) in rendered.iter().enumerate() {
        for (state_b, line_b) in rendered.iter().skip(i + 1) {
            assert!(
                !(line_a == line_b && state_a != state_b),
                "{state_a:?} and {state_b:?} render identically: {line_a}"
            );
        }
    }
}

/// `detected: None` renders as `unknown` and never as `no`, even though
/// `browser_present()` (the install-path predicate) answers `true` for it.
///
/// The two predicates disagree on purpose — `install` must write for a browser
/// it cannot see, `doctor` must not claim it is there — and this pins that the
/// disagreement does not leak into the report.
#[test]
fn an_unknown_presence_is_not_rendered_as_absent() {
    let entry = HostManifestCheck {
        browser: "vivaldi".to_string(),
        support: Some(Support::Supported),
        detected: None,
        manifest: Some(PathBuf::from("/somewhere/com.chat_stasher.host.json")),
        state: HostManifestState::NotRegistered,
    };
    let line = chat_stasher::doctor::host_row_line(&entry);
    assert!(line.contains("unknown"), "{line}");
    assert!(
        !line.contains(" no "),
        "an unknown presence must not be printed as an absence: {line}"
    );
}
