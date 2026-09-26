//! B98 · `install-native-host` (ADR-014 step 2) — the three guards.
//!
//! Everything runs inside a `tempfile` root handed to the command through
//! `--target-root`. No test may ever touch a real browser directory such as
//! `~/Library/Application Support/Google/Chrome/NativeMessagingHosts`, which
//! on the author's machine holds nine other vendors' manifests.
//!
//! Guard 1 — counter-evidence for the old behaviour: before install the
//! manifest is *absent*; after install it exists and every field is asserted
//! literally, including the pinned extension id.
//! Guard 2 — `--uninstall` returns the tree to exactly its pre-install state,
//! and a decoy manifest belonging to another vendor survives byte-identical.
//! Guard 3 — idempotence: two installs, exit 0 both times, exactly one file,
//! identical bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HOST_FILE: &str = "com.chat_stasher.host.json";
const CHROME_ID: &str = "gihmdkkmmmkeiagjjiimacmgkdilofhi";
const GECKO_ID: &str = "chat-stasher@team.iopho.com";
const DECOY: &str = "com.other.vendor.json";
const DECOY_BYTES: &str = "{\"name\":\"com.other.vendor\",\"type\":\"stdio\"}\n";

/// A command that cannot read the real environment even by accident.
fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"));
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run chat-stasher")
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// macOS discovery layout: `<root>/Google/Chrome/NativeMessagingHosts`.
fn chrome_dir(root: &Path) -> PathBuf {
    root.join("Google")
        .join("Chrome")
        .join("NativeMessagingHosts")
}

fn firefox_dir(root: &Path) -> PathBuf {
    root.join("Mozilla").join("NativeMessagingHosts")
}

/// A root pretending Chrome and Firefox are installed and nothing else is.
fn fixture_root(base: &Path) -> PathBuf {
    let root = base.join("app-support");
    fs::create_dir_all(root.join("Google").join("Chrome")).unwrap();
    fs::create_dir_all(root.join("Mozilla")).unwrap();
    root
}

fn install(home: &Path, root: &Path) -> Output {
    run(cli(home).args([
        "install-native-host",
        "--platform",
        "macos",
        "--target-root",
        &root.to_string_lossy(),
    ]))
}

fn list_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

#[test]
fn guard1_manifest_is_absent_before_install_and_exact_after() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());
    let manifest = chrome_dir(&root).join(HOST_FILE);

    // Counter-evidence: the old world, where no host is registered.
    assert!(
        !manifest.exists(),
        "fixture premise failed: {} already exists before install",
        manifest.display()
    );

    let output = install(&home, &root);
    assert_eq!(
        code(&output),
        0,
        "install stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        manifest.is_file(),
        "{} still does not exist after install",
        manifest.display()
    );

    // The command must have said where it wrote, absolutely.
    assert!(
        stdout(&output).contains(&manifest.display().to_string()),
        "output contains no written path, equivalent to silent success:\n{}",
        stdout(&output)
    );

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(value["name"], "com.chat_stasher.host");
    assert_eq!(value["type"], "stdio");
    assert!(
        value["description"]
            .as_str()
            .unwrap()
            .contains("chat-stasher"),
        "description missing: {value}"
    );
    // `path` must be the absolute path of an existing executable.
    let path = PathBuf::from(value["path"].as_str().unwrap());
    assert!(
        path.is_absolute(),
        "path is not absolute: {}",
        path.display()
    );
    assert!(
        path.is_file(),
        "file pointed to by path does not exist: {}",
        path.display()
    );
    // The whole point of ADR-014 step 1: this exact id, with the trailing slash.
    assert_eq!(
        value["allowed_origins"],
        serde_json::json!([format!("chrome-extension://{CHROME_ID}/")])
    );
    assert!(
        value.get("allowed_extensions").is_none(),
        "Chromium manifest should not have allowed_extensions: {value}"
    );
}

#[test]
fn firefox_manifest_uses_allowed_extensions_with_the_gecko_id() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());

    assert_eq!(code(&install(&home, &root)), 0);

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(firefox_dir(&root).join(HOST_FILE)).unwrap())
            .unwrap();
    assert_eq!(value["allowed_extensions"], serde_json::json!([GECKO_ID]));
    assert!(
        value.get("allowed_origins").is_none(),
        "Gecko manifest should not have allowed_origins: {value}"
    );
}

#[test]
fn guard2_uninstall_restores_the_pre_install_state_and_spares_other_vendors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());

    // Another vendor already lives in the shared discovery directory.
    fs::create_dir_all(chrome_dir(&root)).unwrap();
    let decoy = chrome_dir(&root).join(DECOY);
    fs::write(&decoy, DECOY_BYTES).unwrap();

    let before_chrome = list_dir(&chrome_dir(&root));
    let before_firefox = list_dir(&firefox_dir(&root));

    assert_eq!(code(&install(&home, &root)), 0);
    assert!(chrome_dir(&root).join(HOST_FILE).is_file());
    assert!(firefox_dir(&root).join(HOST_FILE).is_file());

    let output = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "macos",
        "--target-root",
        &root.to_string_lossy(),
        "--uninstall",
    ]));
    assert_eq!(
        code(&output),
        0,
        "uninstall stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!chrome_dir(&root).join(HOST_FILE).exists());
    assert!(!firefox_dir(&root).join(HOST_FILE).exists());
    // Byte-for-byte back to the pre-install listing…
    assert_eq!(list_dir(&chrome_dir(&root)), before_chrome);
    assert_eq!(list_dir(&firefox_dir(&root)), before_firefox);
    // …and the directories themselves are still there.
    assert!(chrome_dir(&root).is_dir());
    assert!(firefox_dir(&root).is_dir());
    // …and nothing else was collaterally removed or rewritten.
    assert_eq!(fs::read_to_string(&decoy).unwrap(), DECOY_BYTES);
}

#[test]
fn guard3_installing_twice_leaves_exactly_one_identical_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());

    let first = install(&home, &root);
    assert_eq!(code(&first), 0);
    let after_first = fs::read_to_string(chrome_dir(&root).join(HOST_FILE)).unwrap();

    let second = install(&home, &root);
    assert_eq!(code(&second), 0);
    let after_second = fs::read_to_string(chrome_dir(&root).join(HOST_FILE)).unwrap();

    assert_eq!(after_first, after_second, "second install modified content");
    assert!(
        stdout(&second).contains("unchanged"),
        "second install did not report unchanged:\n{}",
        stdout(&second)
    );
    // Exactly one manifest, and no temp file left behind.
    assert_eq!(list_dir(&chrome_dir(&root)), vec![HOST_FILE.to_string()]);
    assert_eq!(list_dir(&firefox_dir(&root)), vec![HOST_FILE.to_string()]);
}

#[test]
fn browsers_that_are_not_installed_are_skipped_out_loud() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());

    let output = install(&home, &root);
    let text = stdout(&output);
    for absent in [
        "edge",
        "brave",
        "vivaldi",
        "chromium",
        "chrome-canary",
        "arc",
        "chrome-beta",
        "opera",
    ] {
        // The tier is asserted alongside the skip: a browser that is skipped is
        // one thing, and D5's promise about it is another. `arc` is here because
        // D5 adds it to the promised set, and `chrome-beta`/`opera` are here
        // because adding a browser to the matrix without adding it to this list
        // is precisely how a silently-skipped browser gets in.
        let expected = if matches!(
            absent,
            "vivaldi" | "chrome-canary" | "chrome-beta" | "opera"
        ) {
            format!("{absent} (unverified): skipped")
        } else {
            format!("{absent} (supported): skipped")
        };
        assert!(
            text.contains(&expected),
            "{absent} was silently skipped (wanted {expected:?}):\n{text}"
        );
        assert!(
            text.contains("): skipped, browser not installed (no "),
            "a skip must name the path it probed, or the user cannot tell a \
             missing browser from a wrong path:\n{text}"
        );
        assert!(
            !root.join("Microsoft Edge").exists(),
            "skipped browser directory was created"
        );
    }

    // Every browser in the matrix reached the run one way or the other. Without
    // this, a browser added to `Browser::ALL` but not to this test would be
    // neither written nor skipped and nothing would say so.
    for browser in chat_stasher::nativehost::Browser::ALL {
        assert!(
            text.contains(&format!("[install-native-host] {} (", browser.id())),
            "{} never appeared in the install output:\n{text}",
            browser.id()
        );
    }

    // Naming one explicitly overrides the presence probe.
    let forced = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "macos",
        "--target-root",
        &root.to_string_lossy(),
        "--browser",
        "edge",
    ]));
    assert_eq!(code(&forced), 0);
    assert!(root
        .join("Microsoft Edge")
        .join("NativeMessagingHosts")
        .join(HOST_FILE)
        .is_file());
}

#[test]
fn an_empty_root_writes_nothing_and_says_so_with_exit_3() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("empty");
    fs::create_dir_all(&root).unwrap();

    let output = install(&home, &root);
    assert_eq!(code(&output), 3, "empty root must exit 3, not 0");
    assert!(String::from_utf8_lossy(&output.stderr).contains("nothing was written"));
    assert_eq!(
        list_dir(&root),
        Vec::<String>::new(),
        "no artifacts should exist under empty root"
    );
}

#[test]
fn a_malformed_extension_id_is_a_usage_error_not_a_written_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fixture_root(tmp.path());

    // `z` is outside Chromium's a-p alphabet: one wrong character and the
    // browser refuses the host with "native messaging host not found".
    let output = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "macos",
        "--target-root",
        &root.to_string_lossy(),
        "--extension-id",
        &"z".repeat(32),
    ]));
    assert_eq!(code(&output), 2);
    assert!(!chrome_dir(&root).join(HOST_FILE).exists());

    // Same for a host name Chromium's grammar rejects (hyphens are illegal).
    let output = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "macos",
        "--target-root",
        &root.to_string_lossy(),
        "--host-name",
        "com.chat-stasher.host",
    ]));
    assert_eq!(code(&output), 2);
    assert_eq!(list_dir(&chrome_dir(&root)), Vec::<String>::new());
}

/// Linux layout, computed from macOS. Not run on real Linux hardware for this
/// ticket: this asserts the path *shape* the vendor documents describe, which
/// is the part a code change can silently break.
#[test]
fn linux_layout_shape_is_dot_config_and_dot_mozilla() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("linux-home");
    fs::create_dir_all(root.join(".config").join("google-chrome")).unwrap();
    fs::create_dir_all(root.join(".mozilla")).unwrap();

    let output = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "linux",
        "--target-root",
        &root.to_string_lossy(),
    ]));
    assert_eq!(code(&output), 0);
    assert!(root
        .join(".config")
        .join("google-chrome")
        .join("NativeMessagingHosts")
        .join(HOST_FILE)
        .is_file());
    // Firefox spells this directory lowercase-with-hyphens on Linux and
    // CamelCase on macOS. Getting it wrong is invisible until a user reports
    // that the host is never found.
    assert!(root
        .join(".mozilla")
        .join("native-messaging-hosts")
        .join(HOST_FILE)
        .is_file());
}

/// Windows layout + registry argv shape.
///
/// Both arms of the final assertion run on their own platform, so the shape is
/// pinned on Windows CI as well as off it. What is still *not* verified is an
/// actual write to a live registry: this test never executes `reg.exe`, only
/// the printed command line (asserting it targets `HKCU`, never `HKLM`, which
/// would need elevation), plus the manifest JSON landing where Windows looks
/// for it under `LocalAppData`.
#[test]
fn windows_shape_writes_json_and_prints_the_hkcu_registry_command() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("localappdata");
    fs::create_dir_all(&root).unwrap();

    let output = run(cli(&home).args([
        "install-native-host",
        "--platform",
        "windows",
        "--target-root",
        &root.to_string_lossy(),
        "--browser",
        "chrome",
    ]));
    assert_eq!(code(&output), 0);
    let manifest = root
        .join("chat-stasher")
        .join("NativeMessagingHosts")
        .join("chrome")
        .join(HOST_FILE);
    assert!(manifest.is_file());

    let text = stdout(&output);
    assert!(
        text.contains(
            "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\com.chat_stasher.host"
        ),
        "missing HKCU registration command:\n{text}"
    );
    assert!(
        !text.contains("HKLM"),
        "must not touch HKLM (requires privilege elevation):\n{text}"
    );
    // The last line of this command is the one thing that genuinely differs by
    // platform: off Windows there is no registry to write, so the tool must say
    // so rather than imply it did something. Both arms assert — a one-sided
    // `#[cfg]` guard here would mean the Windows arm is never checked at all,
    // which is how a macOS-only `stat -f%z` guard sat in this repo silently
    // never firing on Linux.
    if cfg!(target_os = "windows") {
        assert!(
            !text.contains("registry NOT applied"),
            "on Windows the registry write is real and must not be reported as skipped:\n{text}"
        );
        assert!(
            text.contains("registry "),
            "on Windows the applied registry command must still be reported:\n{text}"
        );
    } else {
        assert!(
            text.contains("registry NOT applied (not running on Windows)"),
            "off Windows the tool must say plainly that the registry step was not done:\n{text}"
        );
    }
}

/// The stub the next ticket regresses against: one line, valid JSON, exit 0.
#[test]
fn native_host_self_test_prints_exactly_one_json_line() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let output = run(cli(&home).args(["native-host", "--self-test"]));
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    // Chromium reads stdout as a u32 frame length: a second line is a bug.
    assert_eq!(lines.len(), 1, "stdout is not exactly one line: {text:?}");
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["host"], "com.chat_stasher.host");
    assert_eq!(value["protocol"], "stdio");
    assert_eq!(value["mode"], "self-test");
    assert_eq!(value["message_loop"], "one-request-per-process");
}

/// Without `--self-test` the subcommand runs the protocol loop.
///
/// **This pair replaces `native_host_without_self_test_refuses_with_exit_2`,
/// which asserted that the subcommand exits 2 with "not implemented" on
/// stderr.** That was true of the build before the framed loop existed; the
/// contract now says bare `native-host` "runs the same one-request loop on
/// stdin/stdout for manual testing" (`contracts/nativehost-protocol.md` §3), so
/// the old stderr line would be a false statement about the binary — the exact
/// failure mode this repository's invariants forbid.
///
/// The old test's purpose — the subcommand must never silently do nothing — is
/// kept and made stronger: a real request is served, and an unreadable stdin
/// produces silence *and* a non-zero status rather than a quiet success.
#[test]
fn native_host_serves_one_request_on_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let stage = tmp.path().join("stage");
    fs::create_dir_all(&stage).unwrap();
    write_stage_config(&home, &stage);

    let body = serde_json::to_vec(&serde_json::json!({"protocol": 1, "type": "hello"})).unwrap();
    let mut frame = (body.len() as u32).to_ne_bytes().to_vec();
    frame.extend_from_slice(&body);

    let mut child = cli(&home)
        .arg("native-host")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(&frame).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = output.stdout;
    assert!(raw.len() >= 4, "stdout is not a frame: {raw:?}");
    let declared = u32::from_ne_bytes(raw[..4].try_into().unwrap()) as usize;
    assert_eq!(raw.len(), 4 + declared, "stdout is not exactly one frame");
    let response: serde_json::Value = serde_json::from_slice(&raw[4..]).unwrap();
    assert_eq!(response["type"], "hello");
    assert_eq!(response["ok"], true);
    assert_eq!(response["stage"], stage.to_string_lossy().as_ref());
}

/// Empty stdin is EOF inside the length prefix: nothing to answer, and no
/// answer is written. Silence must never be exit 0.
#[test]
fn native_host_on_empty_stdin_writes_nothing_and_exits_non_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let output = run(cli(&home).arg("native-host"));
    assert_ne!(
        code(&output),
        0,
        "a host that answered nothing must not report success"
    );
    assert!(
        stdout(&output).is_empty(),
        "diagnostic info must not go to stdout"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).is_empty(),
        "the refusal must be explained on stderr"
    );
}

/// Write the one line the host needs to be able to serve anything.
fn write_stage_config(home: &Path, stage: &Path) {
    // A machine configured for the native host has run the CLI before, so it
    // has an identity. The host itself never creates one
    // (`nativehost::resolve_machine`); seed it the way the CLI does.
    chat_stasher::identity::load_or_create(
        &home
            .join("data")
            .join("chat-stasher")
            .join("machine-identity"),
    )
    .unwrap();
    let dir = home.join("config").join("chat-stasher");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        format!(
            "[native_host]\nstage = {}\n",
            serde_json::to_string(&stage.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
}
