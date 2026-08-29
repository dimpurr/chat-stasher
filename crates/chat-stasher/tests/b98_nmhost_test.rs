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
        "仪器前提失效：安装前 {} 就已经存在",
        manifest.display()
    );

    let output = install(&home, &root);
    assert_eq!(
        code(&output),
        0,
        "install stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(manifest.is_file(), "安装后 {} 仍不存在", manifest.display());

    // The command must have said where it wrote, absolutely.
    assert!(
        stdout(&output).contains(&manifest.display().to_string()),
        "输出里没有写入路径，等于静默成功:\n{}",
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
        "description 缺失: {value}"
    );
    // `path` must be the absolute path of an existing executable.
    let path = PathBuf::from(value["path"].as_str().unwrap());
    assert!(path.is_absolute(), "path 不是绝对路径: {}", path.display());
    assert!(path.is_file(), "path 指向的文件不存在: {}", path.display());
    // The whole point of ADR-014 step 1: this exact id, with the trailing slash.
    assert_eq!(
        value["allowed_origins"],
        serde_json::json!([format!("chrome-extension://{CHROME_ID}/")])
    );
    assert!(
        value.get("allowed_extensions").is_none(),
        "Chromium manifest 不该带 allowed_extensions: {value}"
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
        "Gecko manifest 不该带 allowed_origins: {value}"
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

    assert_eq!(after_first, after_second, "第二次安装改写了内容");
    assert!(
        stdout(&second).contains("unchanged"),
        "第二次安装没有报告 unchanged:\n{}",
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
    for absent in ["edge", "brave", "vivaldi", "chromium", "chrome-canary"] {
        assert!(
            text.contains(&format!("{absent}: skipped")),
            "{absent} 被静默跳过了:\n{text}"
        );
        assert!(
            !root.join("Microsoft Edge").exists(),
            "跳过的浏览器目录被创建了出来"
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
    assert_eq!(code(&output), 3, "空 root 必须是 3, 不是 0");
    assert!(String::from_utf8_lossy(&output.stderr).contains("nothing was written"));
    assert_eq!(
        list_dir(&root),
        Vec::<String>::new(),
        "空 root 下不该有产物"
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
/// ⚠️ UNVERIFIED against a real registry: no Windows machine was available for
/// this ticket. This asserts only what can be asserted from macOS — that the
/// JSON lands somewhere concrete and that the `reg.exe` argv is well formed and
/// per-user (`HKCU`, never `HKLM`, which would need elevation).
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
        "缺少 HKCU 注册命令:\n{text}"
    );
    assert!(!text.contains("HKLM"), "不该动 HKLM (需要提权):\n{text}");
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
    assert_eq!(lines.len(), 1, "stdout 不是恰好一行: {text:?}");
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["host"], "com.chat_stasher.host");
    assert_eq!(value["protocol"], "stdio");
    assert_eq!(value["mode"], "self-test");
    assert_eq!(value["message_loop"], "not-implemented");
}

/// Without `--self-test` the host must refuse rather than pretend to serve.
#[test]
fn native_host_without_self_test_refuses_with_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let output = run(cli(&home).arg("native-host"));
    assert_eq!(code(&output), 2);
    assert!(stdout(&output).is_empty(), "诊断信息不许进 stdout");
    assert!(String::from_utf8_lossy(&output.stderr).contains("not implemented"));
}
