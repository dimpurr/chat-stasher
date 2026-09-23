//! `install-native-host --stage <path>` — recording the host's stage in the
//! config.
//!
//! The config is a *hand-written* file: the shipped template is nothing but
//! comments, and users add their own. So the property under test is not "the key
//! is there" but "the key is there **and the file is otherwise byte-identical**".
//! A serde round-trip would pass the first and fail the second, which is why
//! the assertion here is a prefix comparison against the original bytes rather
//! than a check that some lines still appear somewhere.
//!
//! Nothing here touches a real config, a real stage or a real browser
//! directory: every path is inside one `tempfile` root and is reached through
//! `--target-root` plus `XDG_CONFIG_HOME`/`HOME`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const HOST_FILE: &str = "com.chat_stasher.host.json";

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("home");
        let root = dir.path().join("app-support");
        fs::create_dir_all(&home).expect("home");
        // A discovery root with Chrome present, so the browser half of the
        // command has somewhere to write and does not end in exit 3.
        fs::create_dir_all(root.join("Google").join("Chrome")).expect("chrome dir");
        Fixture {
            _dir: dir,
            home,
            root,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home
            .join("config")
            .join("chat-stasher")
            .join("config.toml")
    }

    fn write_config(&self, text: &str) {
        let path = self.config_path();
        fs::create_dir_all(path.parent().expect("config dir")).expect("mkdir");
        fs::write(&path, text).expect("write config");
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config_path()).expect("read config")
    }

    fn run(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .arg("install-native-host")
            .arg("--platform")
            .arg("macos")
            .arg("--target-root")
            .arg(&self.root)
            .args(extra)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .output()
            .expect("run chat-stasher")
    }

    fn stage(&self) -> PathBuf {
        let stage = self._dir.path().join("stage");
        fs::create_dir_all(&stage).expect("stage");
        stage
    }

    fn code(output: &Output) -> i32 {
        output.status.code().unwrap_or(-1)
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn manifest(&self) -> PathBuf {
        self.root
            .join("Google")
            .join("Chrome")
            .join("NativeMessagingHosts")
            .join(HOST_FILE)
    }
}

/// A config that looks like a user's: the shipped template's comments plus a
/// line of their own.
fn hand_written_config() -> String {
    format!(
        "{}\n# --- my own notes ---\n# I keep my archive on the external disk.\nmachine = \"my-laptop\"\n",
        chat_stasher::config::DEFAULT_CONFIG_TEMPLATE
    )
}

/// The config line that records `stage`, spelled the way TOML spells it.
///
/// Deliberately *not* `format!("{:?}", path)`. That is Rust's escaping of a
/// `str`, and it matches TOML's rendering only while the value contains no
/// backslash — true of every Unix path, and of no Windows path. TOML has two
/// string forms and prefers the literal one (`'C:\Users\…'`) once a value
/// contains a backslash, because `\` is its escape character inside `"…"`;
/// both forms are the same value, and `docs/output-inventory.txt` shows the
/// tool's own warning telling users to prefer the literal one on Windows.
fn stage_line(stage: &str) -> String {
    format!("stage = {}", toml::Value::String(stage.to_owned()))
}

#[test]
fn the_stage_key_is_added_without_touching_a_single_other_byte() {
    let fixture = Fixture::new();
    let before = hand_written_config();
    fixture.write_config(&before);
    let stage = fixture.stage();

    let output = fixture.run(&["--stage", &stage.to_string_lossy()]);
    assert_eq!(
        Fixture::code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Fixture::stdout(&output).contains(&stage.display().to_string()),
        "the command must say which stage it recorded:\n{}",
        Fixture::stdout(&output)
    );
    assert!(
        fixture.manifest().is_file(),
        "the browser half of the command did not run"
    );

    let after = fixture.read_config();
    assert!(
        after.starts_with(&before),
        "the original config was not preserved verbatim.\n--- before ---\n{before}\n--- after ---\n{after}"
    );
    let appended = &after[before.len()..];
    assert!(
        appended.contains("[native_host]"),
        "the new section is missing: {appended:?}"
    );
    assert!(
        appended.contains(&stage_line(&stage.to_string_lossy())),
        "the new key is missing: {appended:?}"
    );
    // Comments inside the original survived (they are part of `before`, which
    // the prefix assertion already covers — this states it in the reader's
    // terms so a future refactor cannot quietly drop them).
    assert!(after.contains("# I keep my archive on the external disk."));
    assert!(after.contains("# chat-stasher configuration"));
    // That the result is still a *readable* config is asserted by
    // `the_recorded_stage_is_what_the_host_then_reports`, which loads it
    // through the same loader the host uses. Checking it here with a second
    // parser would only prove the two parsers agree.
}

#[test]
fn running_it_twice_with_the_same_stage_rewrites_nothing() {
    let fixture = Fixture::new();
    fixture.write_config(&hand_written_config());
    let stage = fixture.stage();

    assert_eq!(
        Fixture::code(&fixture.run(&["--stage", &stage.to_string_lossy()])),
        0
    );
    let after_first = fixture.read_config();

    let second = fixture.run(&["--stage", &stage.to_string_lossy()]);
    assert_eq!(Fixture::code(&second), 0);
    assert!(
        Fixture::stdout(&second).contains("already recorded"),
        "a no-op must say so:\n{}",
        Fixture::stdout(&second)
    );
    assert_eq!(
        fixture.read_config(),
        after_first,
        "a no-op rewrote the config"
    );
}

#[test]
fn changing_the_stage_prints_the_old_value_and_the_new_one() {
    let fixture = Fixture::new();
    fixture.write_config(&hand_written_config());
    let first = fixture.stage();
    let second = {
        let path = fixture._dir.path().join("stage-2");
        fs::create_dir_all(&path).expect("stage 2");
        path
    };

    assert_eq!(
        Fixture::code(&fixture.run(&["--stage", &first.to_string_lossy()])),
        0
    );
    let output = fixture.run(&["--stage", &second.to_string_lossy()]);
    assert_eq!(Fixture::code(&output), 0);

    let text = Fixture::stdout(&output);
    assert!(
        text.contains(&first.display().to_string()),
        "the previous value was not printed:\n{text}"
    );
    assert!(
        text.contains(&second.display().to_string()),
        "the new value was not printed:\n{text}"
    );

    let after = fixture.read_config();
    assert!(after.contains(&stage_line(&second.to_string_lossy())));
    assert!(
        !after.contains(&stage_line(&first.to_string_lossy())),
        "the old value is still in the file"
    );
    assert!(after.contains("# chat-stasher configuration"));
}

#[test]
fn a_stage_path_that_does_not_exist_is_refused_and_the_config_is_untouched() {
    let fixture = Fixture::new();
    let before = hand_written_config();
    fixture.write_config(&before);
    let absent = fixture._dir.path().join("never-created");

    let output = fixture.run(&["--stage", &absent.to_string_lossy()]);
    assert_eq!(
        Fixture::code(&output),
        2,
        "a path that is not a directory is a usage error; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.read_config(),
        before,
        "a refused --stage must leave the config byte-identical"
    );
    assert!(
        !absent.exists(),
        "the command created the stage directory it was told about"
    );
    assert!(
        !fixture.manifest().exists(),
        "nothing may be written before --stage has been validated"
    );
    assert!(
        Fixture::stdout(&output).is_empty() || !Fixture::stdout(&output).contains("[native_host]"),
        "nothing about a stage may be reported as done:\n{}",
        Fixture::stdout(&output)
    );
}

#[test]
fn a_stage_path_that_is_a_file_is_refused_the_same_way() {
    let fixture = Fixture::new();
    let before = hand_written_config();
    fixture.write_config(&before);
    let not_a_dir = fixture._dir.path().join("a-file");
    fs::write(&not_a_dir, "not a directory").expect("write file");

    let output = fixture.run(&["--stage", &not_a_dir.to_string_lossy()]);
    assert_eq!(Fixture::code(&output), 2);
    assert_eq!(fixture.read_config(), before);
}

#[test]
fn a_missing_config_is_created_from_the_template_and_then_gains_the_key() {
    let fixture = Fixture::new();
    let stage = fixture.stage();
    assert!(
        !fixture.config_path().exists(),
        "fixture premise: there is no config yet"
    );

    let output = fixture.run(&["--stage", &stage.to_string_lossy()]);
    assert_eq!(
        Fixture::code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = fixture.read_config();
    assert!(
        after.starts_with(chat_stasher::config::DEFAULT_CONFIG_TEMPLATE),
        "the first-run path must write the shipped template, comments and all"
    );
    assert!(
        after.contains(&stage_line(&stage.to_string_lossy())),
        "the key was not written into the fresh config:\n{after}"
    );
}

/// The shape that made the three cases above pass on macOS and Linux and fail
/// on Windows, reproduced on every platform — this is the test that would have
/// caught the Windows failure without a Windows machine.
///
/// TOML renders a value containing a backslash as a literal string; Rust's
/// `{:?}` renders it as a basic string with every backslash doubled. Every
/// Windows path contains backslashes, and on Unix a directory with one in its
/// name produces the same input, so the writer's escaping branch is reachable
/// here.
#[test]
fn a_stage_path_toml_would_escape_is_recorded_as_the_same_value() {
    let fixture = Fixture::new();
    let before = hand_written_config();
    fixture.write_config(&before);
    let stage = fixture._dir.path().join("stage\\with\\backslash");
    fs::create_dir_all(&stage).expect("stage");
    let stage = stage.to_string_lossy().into_owned();
    assert!(
        stage.contains('\\'),
        "fixture premise: the stage path has a backslash in it"
    );

    let output = fixture.run(&["--stage", &stage]);
    assert_eq!(
        Fixture::code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = fixture.read_config();
    assert!(
        after.starts_with(&before),
        "the original config was not preserved verbatim.\n--- after ---\n{after}"
    );
    let appended = &after[before.len()..];
    assert!(
        appended.contains(&stage_line(&stage)),
        "the new key is missing: {appended:?}"
    );

    // The independent half: read the file back as TOML and compare the value,
    // so the assertion is about the path rather than about which of TOML's two
    // string forms the writer picked.
    let doc: toml::Value = after
        .parse()
        .expect("the config the command wrote must be valid TOML");
    assert_eq!(
        doc["native_host"]["stage"].as_str(),
        Some(stage.as_str()),
        "the recorded stage is not the path it was given"
    );
}

#[test]
fn stage_cannot_be_combined_with_uninstall() {
    let fixture = Fixture::new();
    let before = hand_written_config();
    fixture.write_config(&before);
    let stage = fixture.stage();

    let output = fixture.run(&["--stage", &stage.to_string_lossy(), "--uninstall"]);
    assert_eq!(Fixture::code(&output), 2);
    assert_eq!(fixture.read_config(), before);
}

#[test]
fn a_config_that_is_not_valid_toml_is_reported_and_never_overwritten() {
    let fixture = Fixture::new();
    let broken = "this is not toml at all =\n[[[\n";
    fixture.write_config(broken);
    let stage = fixture.stage();

    let output = fixture.run(&["--stage", &stage.to_string_lossy()]);
    assert_eq!(
        Fixture::code(&output),
        1,
        "a config that cannot be parsed is a failure, not a usage error"
    );
    assert_eq!(
        fixture.read_config(),
        broken,
        "the user's text must survive a parse failure untouched"
    );
}

#[test]
fn the_recorded_stage_is_what_the_host_then_reports() {
    // The two halves of the feature, joined: what `--stage` writes is what
    // `hello` answers with. A test that only checked the file would pass even
    // if the host read a different key.
    let fixture = Fixture::new();
    fixture.write_config(&hand_written_config());
    let stage = fixture.stage();
    assert_eq!(
        Fixture::code(&fixture.run(&["--stage", &stage.to_string_lossy()])),
        0
    );

    let body = serde_json::to_vec(&serde_json::json!({"protocol": 1, "type": "hello"}))
        .expect("serialise");
    let mut frame = (body.len() as u32).to_ne_bytes().to_vec();
    frame.extend_from_slice(&body);

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("native-host")
        .env("HOME", &fixture.home)
        .env("XDG_CONFIG_HOME", fixture.home.join("config"))
        .env("XDG_DATA_HOME", fixture.home.join("data"))
        .env("XDG_STATE_HOME", fixture.home.join("state"))
        .env("XDG_CACHE_HOME", fixture.home.join("cache"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().expect("stdin").write_all(&frame)?;
            child.wait_with_output()
        })
        .expect("run native-host");

    assert_eq!(Fixture::code(&output), 0);
    let declared = u32::from_ne_bytes(output.stdout[..4].try_into().expect("prefix")) as usize;
    assert_eq!(output.stdout.len(), 4 + declared, "stdout is not one frame");
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout[4..]).expect("response JSON");
    assert_eq!(response["type"], "hello");
    assert_eq!(response["machine"], "my-laptop");
    assert_eq!(
        response["stage"].as_str().expect("stage"),
        stage.to_string_lossy()
    );
}
