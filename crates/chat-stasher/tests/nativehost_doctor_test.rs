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
//! written.

use chat_stasher::config::{Config, NativeHostConfig};
use chat_stasher::doctor::{inspect_native_host, HostManifestState, StageConfigCheck};
use chat_stasher::nativehost;
use std::fs;
use std::path::{Path, PathBuf};

/// The manifest path this build would use for Chrome under `home`.
fn chrome_manifest(home: &Path) -> PathBuf {
    let root = nativehost::default_root(nativehost::Platform::current(), home);
    nativehost::target(
        nativehost::Platform::current(),
        &root,
        nativehost::Browser::Chrome,
        nativehost::HOST_NAME,
    )
    .expect("this build knows a Chrome path")
    .manifest
}

fn write_manifest(home: &Path, host_path: &Path) -> PathBuf {
    let manifest = chrome_manifest(home);
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
        }),
        ..Config::default()
    }
}

#[test]
fn an_unregistered_machine_says_so_without_calling_it_broken() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let check = inspect_native_host(&Config::default(), &home);
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
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    write_manifest(&home, &binary);

    let check = inspect_native_host(&Config::default(), &home);
    assert_eq!(chrome_state(&check), HostManifestState::Ok { path: binary });
}

#[test]
fn a_registered_host_whose_binary_is_gone_is_reported_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let gone = dir.path().join("bin").join("chat-stasher");
    write_manifest(&home, &gone);

    let check = inspect_native_host(&Config::default(), &home);
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
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    write_manifest(&home, &binary);

    let check = inspect_native_host(&Config::default(), &home);
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
    write_manifest(&home, &binary);

    let check = inspect_native_host(&Config::default(), &home);
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
    let manifest = chrome_manifest(&home);
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(&manifest, "this is not json").unwrap();

    let check = inspect_native_host(&Config::default(), &home);
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
    let manifest = chrome_manifest(&home);
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(
        &manifest,
        r#"{"name":"com.chat_stasher.host","type":"stdio"}"#,
    )
    .unwrap();

    let check = inspect_native_host(&Config::default(), &home);
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
        inspect_native_host(&Config::default(), &home).stage,
        StageConfigCheck::NotConfigured
    );

    // Configured and present.
    let present = dir.path().join("stage");
    fs::create_dir_all(&present).unwrap();
    assert_eq!(
        inspect_native_host(&config_with_stage(Some(&present)), &home).stage,
        StageConfigCheck::Present {
            path: present.clone()
        }
    );

    // Configured, and nothing is there.
    let absent = dir.path().join("never-created");
    assert_eq!(
        inspect_native_host(&config_with_stage(Some(&absent)), &home).stage,
        StageConfigCheck::Missing {
            path: absent.clone()
        }
    );

    // Configured, and the path is something else entirely.
    let file = dir.path().join("not-a-dir");
    fs::write(&file, "x").unwrap();
    assert_eq!(
        inspect_native_host(&config_with_stage(Some(&file)), &home).stage,
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

    let check = inspect_native_host(&Config::default(), &home);
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
