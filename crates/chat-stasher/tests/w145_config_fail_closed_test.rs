//! W145 — a config file that exists and cannot be used stops the command.
//!
//! The behaviour being pinned: `~/.config/chat-stasher/config.toml` is either
//! usable, absent (the normal first-run state → built-in defaults), or **there
//! and broken** — and the third case runs nothing. It used to warn and carry on
//! with `Config::default()`, which empties `[destinations]`: a scheduled `push`
//! then behaved exactly as if the user had never declared a remote copy, and the
//! archive silently stopped being copied anywhere.
//!
//! Every assertion below is made from the CLI surface (exit status, stderr
//! text, `--json`), never from the library's internals, so the same file is red
//! against the unfixed code and green after it. Nothing here touches a real
//! config, a real archive or a real repository: every run gets a `tempfile`
//! sandbox and a stage directory that is never opened, because the refusal has
//! to happen before any of that.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run the CLI with every XDG directory pointed inside `sandbox`, so a run can
/// neither read the developer's config nor write to their state.
fn isolated_env(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env_remove("CHAT_STASHER_REGISTRY")
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .output()
        .unwrap()
}

/// Write a config file at the location the tool reads, and return its path.
fn write_config(sandbox: &Path, body: &str) -> PathBuf {
    let path = sandbox.join("xdg-config/chat-stasher/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn stage(sandbox: &Path) -> PathBuf {
    let path = sandbox.join("stage");
    fs::create_dir_all(&path).unwrap();
    path
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .expect("the process must exit normally, not by signal")
}

/// Both spellings of "the destinations table is not usable": a value of the
/// wrong type, and a path the tool cannot resolve. Each must stop `push` before
/// it looks at the stage, because the destination list is what `push` would act
/// on — and the old code emptied it and pushed as if nothing were configured.
#[test]
fn push_refuses_on_an_invalid_destination_value() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = stage(sandbox.path());
    let config = write_config(
        sandbox.path(),
        "[destinations.d1]\nrepo = \"/tmp/d1\"\nconnections = \"four\"\n",
    );

    let out = isolated_env(
        sandbox.path(),
        &["push", "--stage", stage.to_str().unwrap()],
    );
    assert_eq!(
        code(&out),
        3,
        "an unusable config is exit 3 (did not finish reading), not a fallback:\n{}",
        combined(&out)
    );
    let text = combined(&out);
    assert!(
        text.contains(&config.display().to_string()),
        "the error must name the file to fix:\n{text}"
    );
    assert!(
        text.contains("line 3"),
        "a TOML value error must carry its position:\n{text}"
    );
    assert!(
        !text.contains("[push] stage check"),
        "push must refuse before it reads the stage:\n{text}"
    );
}

/// An unresolvable path *inside a destination* is the same class of failure and
/// must name the offending option, not just the file: `~alice/repo` is rejected
/// by design (only the current user's `~` is expanded), and the old code dropped
/// the field to `None` and carried on with a destination that had no repo.
#[test]
fn push_refuses_on_a_destination_path_it_cannot_resolve() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = stage(sandbox.path());
    write_config(
        sandbox.path(),
        "[destinations.d1]\nrepo = \"~alice/repo\"\n",
    );

    let out = isolated_env(
        sandbox.path(),
        &["push", "--stage", stage.to_str().unwrap()],
    );
    assert_eq!(code(&out), 3, "{}", combined(&out));
    let text = combined(&out);
    assert!(
        text.contains("destinations.d1.repo"),
        "the error must name the option at fault:\n{text}"
    );
    assert!(
        !text.contains("[push] stage check"),
        "push must refuse before it reads the stage:\n{text}"
    );
}

/// Every command that reads the config refuses — not just `push`. `status` is in
/// this list on purpose: it used to print `config_source=defaults_after_parse_error`
/// and report on defaults, which is the fallback spelled out rather than silent.
#[test]
fn an_invalid_toml_config_stops_every_command_that_reads_it() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = stage(sandbox.path());
    let config = write_config(sandbox.path(), "this is not valid TOML = [\n");

    let invocations: [&[&str]; 4] = [
        &["push", "--stage"],
        &["run-once", "--stage"],
        &["collect", "--stage"],
        &["status"],
    ];
    for args in invocations {
        let mut argv: Vec<&str> = args.to_vec();
        if argv.contains(&"--stage") {
            argv.push(stage.to_str().unwrap());
        }
        let out = isolated_env(sandbox.path(), &argv);
        let text = combined(&out);
        assert_eq!(code(&out), 3, "`{}` must refuse:\n{text}", argv.join(" "));
        assert!(
            text.contains(&config.display().to_string()),
            "`{}` must name the config file:\n{text}",
            argv.join(" ")
        );
        assert!(
            text.contains("line 1"),
            "`{}` must say where the TOML error is:\n{text}",
            argv.join(" ")
        );
        assert!(
            !text.contains("defaults_after_parse_error"),
            "`{}` must not fall back with a marker:\n{text}",
            argv.join(" ")
        );
    }
}

/// The refusal must not become "the tool is broken": with **no** config file the
/// run proceeds on the built-in defaults, exactly as before. Asserted through
/// sections that only a performed run can print (D5 measures the default repo),
/// so a silent refusal or a silent failure cannot pass this test.
#[test]
fn a_missing_config_file_still_runs_on_defaults() {
    let sandbox = tempfile::tempdir().unwrap();
    assert!(!sandbox
        .path()
        .join("xdg-config/chat-stasher/config.toml")
        .exists());

    let out = isolated_env(sandbox.path(), &["doctor"]);
    let text = combined(&out);
    assert_eq!(code(&out), 0, "a healthy first-run machine:\n{text}");
    assert!(
        text.contains("D5 · How much reclaimable garbage is in the repository?"),
        "the reclaim check reads the config and must have run on the defaults:\n{text}"
    );
    assert!(
        !text.contains("cannot be used") && !text.contains("NOT CHECKED"),
        "an absent config file is not an error, and nothing may say it was:\n{text}"
    );
}

/// `doctor` is the one command that does not refuse. It reports the error and
/// names every check it therefore could not perform — and still exits non-zero,
/// because a report of a machine it never inspected must not read as clean.
#[test]
fn doctor_reports_the_error_and_what_it_could_not_check() {
    let sandbox = tempfile::tempdir().unwrap();
    let config = write_config(sandbox.path(), "this is not valid TOML = [\n");

    let out = isolated_env(sandbox.path(), &["doctor"]);
    assert_eq!(code(&out), 3, "{}", combined(&out));
    let text = combined(&out);
    assert!(
        text.contains("config file") && text.contains(&config.display().to_string()),
        "the report must carry the error itself:\n{text}"
    );
    assert!(
        text.contains("NOT CHECKED"),
        "the report must name what it did not check:\n{text}"
    );
    for check in [
        "D3 harness scan",
        "D5 repository reclaim",
        "D6 local metadata cache",
        "D7 destination probes",
        "D8 native host stage",
        "machine identity",
    ] {
        assert!(
            text.contains(check),
            "the skipped-check list must name `{check}`:\n{text}"
        );
    }
    assert!(
        !text.contains("D5 · How much reclaimable garbage"),
        "a skipped check must not print a section as though it had run:\n{text}"
    );
    assert!(
        text.contains("config_source=unreadable"),
        "the provenance marker must say there is no usable config:\n{text}"
    );
}

/// A broken config must not be reported as a broken *registry*.
///
/// `config_unreadable` also sets `scan_failed` (see the JSON test below for why
/// that stays), and D3 used to print its `scan_failed` text on this path too:
/// "🔴 registry missing / unparseable", followed by a pointer to a stderr note
/// ("Refusing to scan with hardcoded roots") that this run never printed. Both
/// sentences claim the run opened the path registry and found it at fault. It
/// never got that far — the config is what failed — and "did not look" is not
/// "found it missing", which is CLAUDE.md invariant 1 stated as text. What the
/// run skipped is asserted as *absent* as well, not just relabelled: a section
/// heading printed under a "did not look" banner reads as a finding with
/// nothing in it ("no risks"), which is the same mistake one line further down.
#[test]
fn doctor_blames_the_config_not_the_registry() {
    let sandbox = tempfile::tempdir().unwrap();
    write_config(sandbox.path(), "this is not valid TOML = [\n");

    let out = isolated_env(sandbox.path(), &["doctor"]);
    assert_eq!(code(&out), 3, "{}", combined(&out));
    let text = combined(&out);
    assert!(
        !text.contains("registry missing"),
        "the registry was never opened on this path, so it must not be blamed:\n{text}"
    );
    assert!(
        !text.contains("Refusing to scan with hardcoded roots"),
        "the report must not point at a stderr note this run never printed:\n{text}"
    );
    assert!(
        !text.contains("D4 · Risk summary"),
        "a skipped section must not print a heading that reads as an empty finding:\n{text}"
    );
    assert!(
        text.contains("D3 · Coverage — NOT CHECKED"),
        "D3 must say it did not run, on the same line a reader looks for a verdict:\n{text}"
    );
    assert!(
        text.contains("never reached the path registry"),
        "D3 must say the registry was never read, not that it was read and is bad:\n{text}"
    );
}

/// The machine-readable half of the same report: `--json` names the error, lists
/// the skipped checks, and turns every config-derived object into
/// `{"checked": false}` rather than an empty finding.
#[test]
fn doctor_json_marks_the_checks_it_did_not_perform() {
    let sandbox = tempfile::tempdir().unwrap();
    let config = write_config(sandbox.path(), "this is not valid TOML = [\n");

    let out = isolated_env(sandbox.path(), &["doctor", "--json"]);
    assert_eq!(code(&out), 3, "{}", combined(&out));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout must be one JSON object");

    assert_eq!(v["config_source"], serde_json::json!("unreadable"));
    // Still `true`, and deliberately: a consumer that predates `config_error`
    // reads only this field, and "coverage unknown" is the honest answer here
    // too. `config_error` is what names *why* — which is why the text report no
    // longer derives its D3 line from this flag (see the test above).
    assert_eq!(v["scan_failed"], serde_json::json!(true));
    let error = v["config_error"]
        .as_str()
        .expect("an unusable config must be reported as a string, not null");
    assert!(
        error.contains(&config.display().to_string()),
        "the JSON error must name the file: {error}"
    );
    let not_checked = v["not_checked"]
        .as_array()
        .expect("not_checked must be an array");
    assert!(
        !not_checked.is_empty(),
        "the skipped checks must be listed: {v}"
    );
    for section in ["reclaim", "cache", "native_host"] {
        assert_eq!(
            v[section]["checked"],
            serde_json::json!(false),
            "`{section}` must say it was not checked, not report an empty finding: {v}"
        );
    }
    // The two checks that read no config were still performed, which is what
    // makes this a report and not just an error message.
    assert!(
        v["claude"]["verdict"].is_object(),
        "D1 reads no config and must still be reported: {v}"
    );
}

/// A machine reading stdout must not have to parse prose: `status --json` still
/// writes exactly one object, and that object says the answer is unknown rather
/// than zero.
#[test]
fn status_json_on_an_unusable_config_is_still_one_object() {
    let sandbox = tempfile::tempdir().unwrap();
    write_config(sandbox.path(), "this is not valid TOML = [\n");

    let out = isolated_env(sandbox.path(), &["status", "--json"]);
    assert_eq!(code(&out), 3, "{}", combined(&out));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout must be one JSON object");
    assert_eq!(v["exit_code"], serde_json::json!(3));
    assert_eq!(v["healthy"], serde_json::json!(false));
    assert_eq!(v["config_source"], serde_json::json!("unreadable"));
    assert!(
        v["config_error"].is_string(),
        "the reason must be in the object: {v}"
    );
    assert_eq!(
        v["scanner"]["kind"],
        serde_json::json!("failed"),
        "nothing was scanned, so the scanner object must say so instead of reporting 0 sessions: {v}"
    );
}

/// The scheduled path is the one that runs unattended, so its failure has to be
/// visible in both channels a sleeping user has: the exit status (what launchd
/// records, and what `status` reads back) and stderr (which the unit redirects
/// to `~/Library/Logs/chat-stasher/run-once.err.log`). The run-state file is
/// asserted as well, because "the timer failed at the config step" is the
/// sentence the user reads the next morning.
#[test]
fn a_scheduled_run_refuses_loudly_and_records_the_step() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = stage(sandbox.path());
    write_config(sandbox.path(), "this is not valid TOML = [\n");

    let out = isolated_env(
        sandbox.path(),
        &["run-once", "--stage", stage.to_str().unwrap()],
    );
    let text = combined(&out);
    assert_eq!(code(&out), 3, "{text}");
    assert!(
        text.contains("[run-once] result: ERROR exit_code=3 config="),
        "the scheduled run must log its failure on the line the log reader looks at:\n{text}"
    );
    assert!(
        !text.contains("[collect]"),
        "nothing may be collected on a config it could not read:\n{text}"
    );

    let state = sandbox
        .path()
        .join("xdg-data/chat-stasher/state/run-state.json");
    let raw = fs::read_to_string(&state).expect("the run state must be recorded even on refusal");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["outcome"], serde_json::json!("error"));
    assert_eq!(
        v["failed_step"],
        serde_json::json!("config"),
        "the recorded step must name the config, not a later stage: {raw}"
    );
    assert_eq!(
        v["shards_written"],
        serde_json::json!(0),
        "no shard can have been written: {raw}"
    );
}
