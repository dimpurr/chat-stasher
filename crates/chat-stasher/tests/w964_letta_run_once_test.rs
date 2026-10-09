//! W964 — LETTA-P3 through the real binary: a `[pull.letta]` declaration
//! makes the scheduled pass pull the declared producer before its collect
//! step, and a producer that cannot finish reading keeps the whole pass at
//! exit 3 while the local archive work still happens.
//!
//! Black-box like [run_once_activity_index_test]: every ambient path is
//! redirected into a sandbox and the real `chat-stasher` binary runs. The
//! producer has no credential in this environment, so its refusal happens
//! before any network attempt — which is exactly the observable the exit-3
//! promise is made of: the pass did the local duty (snapshot created) and
//! still refuses to look like success while a declared producer went unread,
//! the way a pass that pushed "no messages" would.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W964 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("LETTA_API_KEY")
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

fn write_shard(stage: &Path, machine: &str, session: &str) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("000001.jsonl"),
        r#"{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2025-01-15T12:34:56.789Z","cwd":"/x","version":"1.0.31"}"#
            .to_string()
            + "\n",
    )
    .unwrap();
}

/// One pass with a declared producer and no credential: the local duty must
/// still complete (a snapshot, a repository) while the exit code stays 3 and
/// the result line names the unread producer — the pass "did not finish
/// reading", so nothing about Letta can be read as "no messages".
#[test]
fn run_once_with_a_declared_but_unreadable_producer_archives_and_exits_3() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let inbox = sandbox.join("letta-inbox");
    write_shard(&stage, machine, session);

    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "push_only_if_changed = false\n\n[pull.letta]\naccount_id = \
             \"synthetic-account\"\ninbox = \"{}\"\n",
            inbox.display()
        ),
    )
    .unwrap();

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The producer never read anything: no credential exists, and the pass
    // never offers the unread producer as "no messages" — it exits 3 and
    // names the failed step.
    assert_eq!(
        out.status.code(),
        Some(3),
        "an unread declared producer keeps the pass at exit 3; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[run-once] pull letta incomplete"),
        "the refusal names the producer step:\n{stderr}"
    );
    assert!(
        stderr.contains("[run-once] result: ERROR exit_code=3 pull_letta snapshot=created"),
        "the result line names the failed step next to the created snapshot:\n{stderr}"
    );

    // The local duty still ran to completion inside the same pass.
    assert!(
        stdout.contains("[run-once] activity-index: sessions=1"),
        "collect, index and push still ran:\n{stdout}\n{stderr}"
    );
    assert!(
        repo.exists(),
        "the local snapshot was pushed even though the producer went unread"
    );

    // The durable record agrees: a failed pass whose producer step errored.
    let state = fs::read_to_string(
        sandbox
            .join("data")
            .join("chat-stasher")
            .join("state")
            .join("run-state.json"),
    )
    .unwrap();
    assert!(
        state.contains("\"failed_step\": \"pull-letta\""),
        "run-state names the failed step:\n{state}"
    );
    assert!(
        state.contains("\"snapshot_created\": true"),
        "run-state still records the created snapshot:\n{state}"
    );
    assert!(
        state.contains("\"letta_pull_ms\""),
        "the pull phase is in the phase record:\n{state}"
    );
    let summary = stdout
        .lines()
        .find(|line| line.starts_with("[run-once] phases ms:"))
        .expect("the phases summary line is printed last");
    assert!(
        summary.contains("letta_pull="),
        "the summary line carries the pull phase:\n{summary}"
    );
}

/// Without a declaration the pass is unchanged: no pull step, no producer
/// diagnostics, the ordinary completed result and exit 0.
#[test]
fn run_once_without_a_declaration_stays_a_complete_pass() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(&stage, machine, session);

    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "push_only_if_changed = false\n",
    )
    .unwrap();

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(
        out.status.success(),
        "no declaration means the pass's behaviour is unchanged"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("pull letta") && !stderr.contains("pull letta"),
        "no producer step ran:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("[run-once] result: COMPLETED snapshot=created exit_code=0"),
        "the ordinary completed result stands:\n{stdout}"
    );
    let summary = stdout
        .lines()
        .find(|line| line.starts_with("[run-once] phases ms:"))
        .expect("the phases summary line is printed last");
    assert!(
        summary.contains("letta_pull=0"),
        "an undeclared pull is a recorded zero:\n{summary}"
    );
}

/// A declaration that cannot be honoured (a page size outside the served
/// band, a missing account) is a config the pass refuses to interpret, and
/// the refusal names the key — never a silent producer that never runs.
#[test]
fn run_once_refuses_an_invalid_declaration_naming_the_key() {
    for (bad, naming) in [
        (
            "[pull.letta]\naccount_id = \"synthetic-account\"\n\
             inbox = \"/tmp/w964-inbox\"\npage_size = 500\n",
            "pull.letta.page_size",
        ),
        (
            "[pull.letta]\naccount_id = \"synthetic-account\"\n",
            "pull.letta.inbox",
        ),
    ] {
        let sb = tempfile::tempdir().unwrap();
        let sandbox = sb.path();
        let stage = sandbox.join("stage");
        write_shard(
            &stage,
            "mbp-test",
            "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        );
        let config_dir = sandbox.join("config").join("chat-stasher");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), bad).unwrap();
        let out = run(
            sandbox,
            &[
                "run-once",
                "--stage",
                stage.to_str().unwrap(),
                "--machine",
                "mbp-test",
                "--repo",
                sandbox.join("repo").to_str().unwrap(),
                "--key-file",
                sandbox
                    .join("keys")
                    .join("masterkey.json")
                    .to_str()
                    .unwrap(),
            ],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(3),
            "an invalid declaration holds the pass at exit 3; stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains(naming),
            "the refusal names the key at fault:\n{stderr}"
        );
        assert!(
            stderr.contains("pull_letta"),
            "the result names the producer step:\n{stderr}"
        );
    }
}
