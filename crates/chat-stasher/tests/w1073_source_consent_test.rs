//! W1073 / ADR-065 — source consent, driven through the real binary.
//!
//! Every test runs the built `chat-stasher` with an isolated HOME and the
//! shipped registry (no `CHAT_STASHER_REGISTRY`), against synthetic session
//! files whose content is a canary string. Nothing here is a unit call on the
//! filter: a source the user did not choose must leave zero bytes behind, and
//! the proof is a walk over every file the run created.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

const WANTED: &str = "WANTED_CLAUDE_CANARY_41c0";
const UNCHOSEN: &str = "UNCHOSEN_CODEX_CANARY_7f3a";
const LATE: &str = "LATE_GEMINI_CANARY_92be";

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        Sandbox { root }
    }
    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    fn stage(&self) -> PathBuf {
        self.root.path().join("stage")
    }
    fn config_path(&self) -> PathBuf {
        self.root.path().join("xdg-config/chat-stasher/config.toml")
    }
    fn write_config(&self, text: &str) {
        let path = self.config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn claude_session(&self) {
        let dir = self.home().join(".claude/projects/-fixture-project");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("11111111-1111-4111-8111-111111111111.jsonl"),
            format!("{{\"type\":\"user\",\"message\":\"{WANTED}\"}}\n"),
        )
        .unwrap();
    }
    fn codex_session(&self, base: &Path) {
        let dir = base.join("2026/11/01");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("rollout-2026-11-01T00-00-00-22222222-2222-4222-8222-222222222222.jsonl"),
            format!("{{\"type\":\"event\",\"message\":\"{UNCHOSEN}\"}}\n"),
        )
        .unwrap();
    }
    fn gemini_session(&self) {
        let dir = self.home().join(".gemini/tmp/fixtureproject/chats");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("session-2026-11-02T00-00-aaaa.json"),
            format!("{{\"messages\":[{{\"content\":\"{LATE}\"}}]}}"),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        let home = self.home();
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        // The shipped registry, unless a test planted its own.
        let registry = self.root.path().join("registry.json");
        if registry.exists() {
            command.env("CHAT_STASHER_REGISTRY", registry);
        } else {
            command.env_remove("CHAT_STASHER_REGISTRY");
        }
        command
            .args(args)
            .env("HOME", &home)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&home),
            )
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", self.root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.path().join("xdg-data"))
            .env("XDG_STATE_HOME", self.root.path().join("xdg-state"))
            .env_remove("CODEX_HOME")
            .env_remove("GEMINI_CLI_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("OPENCODE_DB")
            .output()
            .expect("run chat-stasher")
    }
    fn collect(&self) -> Output {
        let stage = self.stage();
        self.run(&[
            "collect",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "w1073",
        ])
    }
    fn run_once(&self) -> Output {
        let stage = self.stage();
        self.run(&[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "w1073",
        ])
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn all_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            all_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// True when `needle` appears in any file name or file body under `root`.
fn tree_contains(root: &Path, needle: &str) -> bool {
    let mut files = Vec::new();
    all_files(root, &mut files);
    files.iter().any(|path| {
        path.to_string_lossy().contains(needle)
            || fs::read(path)
                .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
    })
}

fn staged_harnesses(stage: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(machines) = fs::read_dir(stage.join("sessions")) else {
        return out;
    };
    for machine in machines.flatten() {
        for session in fs::read_dir(machine.path()).unwrap().flatten() {
            let name = session.file_name().to_string_lossy().into_owned();
            out.push(name.split('.').next().unwrap().to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An unchosen source leaves zero bytes in the stage and in everything the
/// pass wrote (the local archive repository lives under the isolated data dir).
#[test]
fn an_unchosen_source_leaves_zero_bytes_in_stage_and_archive() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.codex_session(&sb.home().join(".codex/sessions"));
    sb.write_config("[sources]\nclaude-code = \"on\"\ncodex = \"off\"\n");

    let out = sb.run_once();
    assert!(out.status.success(), "run-once failed: {}", text(&out));

    assert_eq!(
        staged_harnesses(&sb.stage()),
        vec!["claude-code".to_string()]
    );
    assert!(
        !tree_contains(&sb.stage(), UNCHOSEN),
        "canary of the unchosen source reached the stage"
    );
    assert!(
        !tree_contains(&sb.root.path().join("xdg-data"), UNCHOSEN),
        "canary of the unchosen source reached the archive or state"
    );
    // The chosen source's session did land: the walk above can see staged files.
    assert!(
        tree_contains(&sb.stage(), WANTED),
        "the chosen source's session should be staged (the control for the walk)"
    );
}

/// A tool that appears after the choice stays pending across a real run-once.
#[test]
fn a_tool_that_appears_later_stays_pending() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.write_config("[sources]\nclaude-code = \"on\"\n");
    let first = sb.run_once();
    assert!(first.status.success(), "{}", text(&first));

    sb.gemini_session();
    let second = sb.run_once();
    assert!(second.status.success(), "{}", text(&second));
    let combined = text(&second);
    assert!(
        combined.contains("[sources] pending") && combined.contains("gemini-cli"),
        "the pass must name the pending tool: {combined}"
    );
    assert!(!tree_contains(&sb.stage(), LATE));
    assert!(!staged_harnesses(&sb.stage()).contains(&"gemini-cli".to_string()));
}

/// No `[sources]` table and a stage that already holds a source: that source
/// stays on, nothing else turns on, and the pass says so once.
#[test]
fn the_upgrade_rule_keeps_what_the_stage_already_holds() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.write_config("[sources]\nclaude-code = \"on\"\n");
    let first = sb.collect();
    assert!(first.status.success(), "{}", text(&first));

    // Simulate the machine before this release: no table, a populated stage.
    sb.write_config("# an old config with no [sources] table\n");
    sb.codex_session(&sb.home().join(".codex/sessions"));
    let out = sb.collect();
    assert!(out.status.success(), "{}", text(&out));
    let combined = text(&out);
    assert!(
        combined.contains("no [sources] table is recorded yet") && combined.contains("claude-code"),
        "the upgrade notice is missing: {combined}"
    );
    assert!(!tree_contains(&sb.stage(), UNCHOSEN));
    assert_eq!(
        staged_harnesses(&sb.stage()),
        vec!["claude-code".to_string()]
    );
}

/// A root the user declared is a statement (on); an explicit off beats it.
#[test]
fn off_beats_a_declared_root_and_a_declared_root_alone_is_on() {
    let sb = Sandbox::new();
    let codex_root = sb.root.path().join("declared-codex");
    sb.codex_session(&codex_root);
    sb.claude_session();
    let root = codex_root.to_str().unwrap().replace('\\', "/");

    sb.write_config(&format!(
        "[sources]\nclaude-code = \"on\"\ncodex = \"off\"\n[harness_roots]\ncodex = \"{root}\"\n"
    ));
    let out = sb.collect();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !tree_contains(&sb.stage(), UNCHOSEN),
        "off must beat a declared root"
    );

    sb.write_config(&format!(
        "[sources]\nclaude-code = \"on\"\n[harness_roots]\ncodex = \"{root}\"\n"
    ));
    let out = sb.collect();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        staged_harnesses(&sb.stage()).contains(&"codex".to_string()),
        "a declared root with no [sources] entry counts as on"
    );
}

/// With nothing recorded, collect and run-once refuse with exit 4 and name the
/// next command; nothing is written to the stage.
#[test]
fn no_recorded_consent_refuses_with_its_own_exit_code() {
    let sb = Sandbox::new();
    sb.claude_session();
    for out in [sb.collect(), sb.run_once()] {
        assert_eq!(out.status.code(), Some(4), "{}", text(&out));
        let combined = text(&out);
        assert!(combined.contains("chat-stasher setup"), "{combined}");
        assert!(!tree_contains(&sb.stage(), WANTED));
    }
}

/// `status --json` keeps off, pending, on, and not-found apart.
#[test]
fn status_keeps_off_pending_and_not_found_apart() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.codex_session(&sb.home().join(".codex/sessions"));
    sb.gemini_session();
    sb.write_config("[sources]\nclaude-code = \"on\"\ncodex = \"off\"\n");
    let out = sb.run(&["status", "--json"]);
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&out)));
    let state_of = |id: &str| -> String {
        value["sources"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .map(|s| s["state"].as_str().unwrap().to_string())
            .unwrap_or_default()
    };
    assert_eq!(state_of("claude-code"), "on");
    assert_eq!(state_of("codex"), "off");
    assert_eq!(state_of("gemini-cli"), "pending");
    let pending_present: Vec<_> = value["sources"]["pending_present"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(pending_present.contains(&"gemini-cli".to_string()));
    // Not found is a different word from off and pending: a tool that is not
    // installed is pending-but-absent, never listed as present.
    assert!(!pending_present.contains(&"cursor".to_string()));
    // The human form says the same, and calls neither a gap.
    let human = sb.run(&["status"]);
    let human = text(&human);
    assert!(human.contains("off, by your choice: codex"), "{human}");
    assert!(
        human.contains("pending, not collected until you decide: gemini-cli"),
        "{human}"
    );
    assert!(human.contains("codex 1 (off)"), "{human}");
}

impl Sandbox {
    fn setup(&self, extra: &[&str]) -> Output {
        let stage = self.stage();
        let mut args = vec![
            "setup",
            "--stage",
            stage.to_str().unwrap(),
            "--masterkey-saved-elsewhere",
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }
    fn config_text(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap_or_default()
    }
}

/// `setup --sources` records the answer and collects only what it names.
#[test]
fn setup_sources_collects_only_the_named_source_and_records_the_rest_off() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.codex_session(&sb.home().join(".codex/sessions"));
    let out = sb.setup(&["--sources", "claude-code"]);
    let combined = text(&out);
    let config = sb.config_text();
    assert!(
        config.contains("claude-code = \"on\""),
        "{config}\n{combined}"
    );
    assert!(config.contains("codex = \"off\""), "{config}");
    assert_eq!(
        staged_harnesses(&sb.stage()),
        vec!["claude-code".to_string()]
    );
    assert!(!tree_contains(&sb.stage(), UNCHOSEN));
}

/// `setup --all-detected` is the explicit "everything found".
#[test]
fn setup_all_detected_collects_every_found_source() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.codex_session(&sb.home().join(".codex/sessions"));
    let out = sb.setup(&["--all-detected"]);
    assert_eq!(
        staged_harnesses(&sb.stage()),
        vec!["claude-code".to_string(), "codex".to_string()],
        "{}",
        text(&out)
    );
}

/// No TTY and nothing recorded: exit 4, the next command, and nothing written.
#[test]
fn setup_with_no_tty_and_no_choice_refuses_and_writes_nothing() {
    let sb = Sandbox::new();
    sb.claude_session();
    let stage = sb.stage();
    let out = sb.run(&["setup", "--json", "--stage", stage.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["exit_code"], 4);
    assert_eq!(value["refusal"]["kind"], "no_source_consent");
    let next = value["refusal"]["next"].as_str().unwrap();
    assert!(
        next.contains("--all-detected") && next.contains("--stage"),
        "{next}"
    );
    assert!(
        !sb.config_path().exists(),
        "the refusal must not write a config"
    );
    assert!(!stage.exists(), "the refusal must not create the stage");
    assert!(!sb.root.path().join("xdg-data/chat-stasher/repo").exists());
}

/// An unknown id in `--sources` is a usage error.
#[test]
fn setup_sources_with_an_unknown_id_is_a_usage_error() {
    let sb = Sandbox::new();
    let out = sb.setup(&["--sources", "not-a-tool"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(!sb.config_path().exists());
}

/// `sources` edits touch only `[sources]`: every other byte survives.
#[test]
fn a_sources_edit_keeps_the_users_comments_and_order() {
    let sb = Sandbox::new();
    let head = "# my own note about this machine\nmachine = \"w1073\" # pinned\n\n# the table\n";
    let table = "[sources]\n# work laptop: never codex\ncodex = \"on\" # was on\n";
    let tail = "\n# roots below\n[harness_roots]\n# keep me\nzed = \"/fixture/zed\"\n";
    sb.write_config(&format!("{head}{table}{tail}"));

    let off = sb.run(&["sources", "disable", "codex"]);
    assert_eq!(off.status.code(), Some(0), "{}", text(&off));
    let on = sb.run(&["sources", "enable", "claude-code"]);
    assert_eq!(on.status.code(), Some(0), "{}", text(&on));

    let expected = format!(
        "{head}[sources]\n# work laptop: never codex\ncodex = \"off\" # was on\n\
         claude-code = \"on\"\n{tail}"
    );
    assert_eq!(sb.config_text(), expected);

    // D9: off is not delete, and the command says so.
    let notice = text(&off);
    assert!(notice.contains("Nothing was deleted"), "{notice}");
    assert!(notice.contains("plaintext"), "{notice}");
    assert!(notice.contains("reclaim-stage"), "{notice}");
    assert!(notice.contains("chat-stasher purge"), "{notice}");
}

/// `sources enable` on an id the registry does not list is exit 2.
#[test]
fn sources_with_an_unknown_id_is_a_usage_error() {
    let sb = Sandbox::new();
    let out = sb.run(&["sources", "enable", "not-a-tool"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(!sb.config_path().exists());
}

/// D7: the first `sources` write records what the upgrade rule carried over,
/// so writing the table does not silently stop a source that was collecting.
#[test]
fn the_first_sources_write_records_carried_over_sources_explicitly() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.write_config("[sources]\nclaude-code = \"on\"\n");
    assert!(sb.collect().status.success());
    sb.write_config("# no table yet\n");
    let stage = sb.stage();
    let out = sb.run(&[
        "sources",
        "--stage",
        stage.to_str().unwrap(),
        "disable",
        "zed",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let config = sb.config_text();
    assert!(config.contains("claude-code = \"on\""), "{config}");
    assert!(config.contains("zed = \"off\""), "{config}");
}

/// The listing keeps off, on, not found and pending apart, and reads no text.
#[test]
fn the_sources_listing_keeps_off_and_not_found_apart() {
    let sb = Sandbox::new();
    sb.claude_session();
    sb.codex_session(&sb.home().join(".codex/sessions"));
    sb.write_config("[sources]\nclaude-code = \"on\"\ncodex = \"off\"\n");
    let out = sb.run(&["sources"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = |id: &str| {
        listing
            .lines()
            .find(|l| l.contains(id) && l.contains("sessions"))
            .unwrap_or_default()
            .to_string()
    };
    assert!(line("codex").trim_start().starts_with("off"), "{listing}");
    assert!(
        line("claude-code").trim_start().starts_with("on"),
        "{listing}"
    );
    let not_found = listing
        .lines()
        .find(|l| l.contains("Not found on this machine"))
        .unwrap_or_default();
    assert!(not_found.contains("gemini-cli"), "{listing}");
    assert!(
        !not_found.contains("codex"),
        "off is not not-found: {listing}"
    );
    assert!(!listing.contains(UNCHOSEN) && !listing.contains(WANTED));
}

/// Sealing already-staged data is not collection: a source turned off keeps it.
#[test]
fn seal_of_stage_data_keeps_working_for_a_source_turned_off() {
    let sb = Sandbox::new();
    let cell = serde_json::json!({
        "template": "~/x/",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "W1073 fixture"
    });
    let registry = sb.root.path().join("registry.json");
    fs::write(
        &registry,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "W1073",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "Claude Code",
                "seal_policy": "rename",
                "seal_source": "W1073 fixture",
                "paths": {"macos": cell, "linux": cell, "windows": cell}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    sb.write_config("[sources]\nclaude-code = \"off\"\n");
    let stage = sb.stage();
    fs::create_dir_all(&stage).unwrap();
    let active = stage.join("active.jsonl");
    fs::write(&active, b"{\"synthetic\":true}\n").unwrap();
    let out = sb.run(&[
        "seal",
        "--harness",
        "claude-code",
        "--active",
        active.to_str().unwrap(),
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "w1073",
        "--session",
        "claude-code.w1073.fixture",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(staged_harnesses(&stage).contains(&"claude-code".to_string()));
}

/// The app-driven backfill is a route to one named source, so it obeys the same
/// gate. Off macOS it stops earlier on its own platform refusal (exit 3), which
/// is a fact about the app, not about consent.
#[test]
fn backfill_refuses_a_source_that_is_not_on() {
    let sb = Sandbox::new();
    sb.write_config("[sources]\ngrok-bot = \"off\"\n");
    let stage = sb.stage();
    fs::create_dir_all(&stage).unwrap();
    let out = sb.run(&[
        "backfill",
        "grok-bot",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "w1073",
    ]);
    let want = if cfg!(target_os = "macos") { 4 } else { 3 };
    assert_eq!(out.status.code(), Some(want), "{}", text(&out));
    if cfg!(target_os = "macos") {
        assert!(
            text(&out).contains("chat-stasher sources enable grok-bot"),
            "{}",
            text(&out)
        );
    }
}

#[test]
fn extension_consent_does_not_create_doctor_gaps() {
    let sb = Sandbox::new();
    let stage = sb.stage();
    sb.write_config(&format!(
        "[native_host]\nstage = {:?}\n",
        stage.to_str().unwrap()
    ));
    let reports = stage.join("ext-status").join("fixture-machine");
    fs::create_dir_all(&reports).unwrap();
    let path = reports.join("fixture-install.json");
    let write = |state: &str| {
        fs::write(
            &path,
            serde_json::json!({
                "schema":"chat-stasher/ext-status@1", "machine":"fixture-machine",
                "install_id":"fixture-install", "reported_at":"2026-01-01T00:00:00Z",
                "consent":{"version":1,"platforms":{"alpha":{"state":state,"updatedAt":1000}}}
            })
            .to_string(),
        )
        .unwrap();
    };
    write("on");
    let on = sb.run(&["doctor", "--json"]);
    let on: serde_json::Value = serde_json::from_slice(&on.stdout).unwrap();
    write("off");
    let off = sb.run(&["doctor", "--json"]);
    let off: serde_json::Value = serde_json::from_slice(&off.stdout).unwrap();
    assert_eq!(on["archive_gaps"], off["archive_gaps"]);
}
