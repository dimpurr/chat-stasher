//! W219 end-to-end: real shard files on disk → `activity-index` → the account
//! keys → the conversation verdict.
//!
//! The unit tests in `activity.rs` and `overview.rs` call `build_row` and
//! `conversation_identities` with strings and structs they build themselves.
//! That leaves one link unproven: whether the `account` envelope the extension
//! writes on a **sealed shard line** actually reaches the row, through the real
//! index file and its serde round trip. A field that is extracted correctly in
//! a unit test and never wired into `rebuild_activity_index` would pass every
//! one of those tests and be absent from every archive.
//!
//! So this file drives the real binary over a real stage and reads the real
//! index files back. It deliberately stops at the index: the step from there
//! into a repository groups snapshots by **snapshot hostname** and reads only
//! the newest per hostname, and this harness cannot give two pushes two
//! hostnames (the platform sets that field, and nothing in this repository
//! overrides it). That step is `readback::newest_snapshot_per_host`, it is
//! pre-existing, and it is not what W219 changed.
//!
//! Nothing here is conversation text: synthetic shard lines, opaque fingerprints.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W219 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// A 64-hex value, the shape `contracts/inbox.schema.json` requires of a
/// fingerprint. Opaque: it stands for an HMAC this test never computes.
const VALUE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const VALUE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// One sealed shard line, spelled the way the extension writes it: the `@2`
/// bundle schema, an `account` envelope, and a raw body.
fn shard_line(session_id: &str, account: &str) -> String {
    format!(
        r#"{{"schema":"chat-stasher/inbox@2","kind":"bundle","id":"deepseek.{session_id}","platform":"deepseek","session_id":"{session_id}","captured_at":"2026-01-15T12:34:56Z","account":{account},"raw":{{"text":"synthetic","bytes":9}}}}"#
    )
}

fn fingerprint(salt: &str, value: &str) -> String {
    format!(
        r#"{{"kind":"fingerprint","value":"{value}","source":"response-body-platform-uid","saltId":"{salt}"}}"#
    )
}

/// Write one sealed shard for a session named `session` under `machine`.
fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// Read one machine's index file back as rows, through the same conversion the
/// read path uses.
fn rows_from_index(stage: &Path, machine: &str) -> Vec<chat_stasher::overview::OverviewRow> {
    let path = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let row: chat_stasher::activity::ActivityRow =
                serde_json::from_str(line).expect("an index line round-trips");
            chat_stasher::sidecar::to_overview_row(&row)
        })
        .collect()
}

fn index_stage(sandbox: &Path, stage: &Path, machine: &str) {
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed for {machine}: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The `account` envelope on a sealed shard line reaches the index row, and
/// from there the verdict — the one link the unit tests cannot see.
///
/// Three conversations across two machines, chosen so one fact is pinned per
/// row: one id carrying **two accounts under one salt** (the collision), one id
/// seen on **two machines** with incomparable salts (not a collision, but one
/// conversation), and one id whose account is `unknown` (no key at all).
#[test]
fn account_keys_survive_the_index_and_decide_the_conversation() {
    let sb = tempfile::TempDir::new().unwrap();
    let stage = sb.path().join("stage");
    let (mbp, air) = ("mbp-w219", "air-w219");
    let (shared, collided, unknown) = (
        "11111111-0000-0000-0000-000000000001",
        "22222222-0000-0000-0000-000000000002",
        "33333333-0000-0000-0000-000000000003",
    );

    write_shard(
        &stage,
        mbp,
        &format!("deepseek.{shared}"),
        &[shard_line(shared, &fingerprint("salt-mbp", VALUE_A))],
    );
    write_shard(
        &stage,
        mbp,
        &format!("deepseek.{collided}"),
        &[shard_line(collided, &fingerprint("salt-mbp", VALUE_A))],
    );
    write_shard(
        &stage,
        mbp,
        &format!("deepseek.{unknown}"),
        &[shard_line(
            unknown,
            r#"{"kind":"unknown","reason":"no-account-id-in-capture"}"#,
        )],
    );
    write_shard(
        &stage,
        air,
        &format!("deepseek.{shared}"),
        &[shard_line(shared, &fingerprint("salt-air", VALUE_B))],
    );
    write_shard(
        &stage,
        air,
        &format!("deepseek.{collided}"),
        &[shard_line(&collided, &fingerprint("salt-mbp", VALUE_B))],
    );

    index_stage(sb.path(), &stage, mbp);
    index_stage(sb.path(), &stage, air);

    let rows: Vec<_> = rows_from_index(&stage, mbp)
        .into_iter()
        .chain(rows_from_index(&stage, air))
        .collect();
    assert_eq!(rows.len(), 5, "one row per machine per session directory");

    // The shared id: two machines, two salts, so two incomparable values.
    let shared_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.session_id.ends_with(shared))
        .collect();
    assert_eq!(shared_rows.len(), 2);
    for r in &shared_rows {
        assert_eq!(r.account_keys.len(), 1, "the envelope reached the row");
        assert!(
            r.account_keys[0].value == VALUE_A || r.account_keys[0].value == VALUE_B,
            "and it is the value the shard line carried"
        );
        assert!(
            r.account_keys[0].salt_id == "salt-mbp" || r.account_keys[0].salt_id == "salt-air",
            "the salt travels with it"
        );
    }

    // The collided id: one salt, two values.
    let collided_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.session_id.ends_with(collided))
        .collect();
    assert_eq!(collided_rows.len(), 2);
    assert!(
        collided_rows
            .iter()
            .all(|r| r.account_keys.len() == 1 && r.account_keys[0].salt_id == "salt-mbp"),
        "both records were written under one salt"
    );

    // An unknown account contributes no key, so it can neither agree nor
    // disagree with a fingerprint.
    let unknown_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.session_id.ends_with(unknown))
        .collect();
    assert_eq!(unknown_rows.len(), 1);
    assert!(
        unknown_rows[0].account_keys.is_empty(),
        "`unknown` is not a value: {:?}",
        unknown_rows[0].account_keys
    );

    // ---- and the verdict over exactly these rows -------------------------
    use chat_stasher::overview::{
        conversation_count, conversation_identities, cross_machine_count, AccountVerdict,
        CollidingSalt,
    };
    assert_eq!(
        conversation_count(&rows),
        3,
        "three conversations, five rows"
    );
    assert_eq!(
        cross_machine_count(&rows),
        2,
        "two ids live on both machines"
    );

    let ids = conversation_identities(&rows);
    let verdict = |suffix: &str| {
        ids.iter()
            .find(|c| c.session_id.ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} is in the verdict"))
            .accounts
            .clone()
    };
    assert_eq!(
        verdict(unknown),
        AccountVerdict::NotRecorded,
        "no key ⇒ nothing to compare ⇒ no claim"
    );
    assert_eq!(
        verdict(shared),
        AccountVerdict::Consistent,
        "two salts are not comparable, so two values are not a collision"
    );
    assert_eq!(
        verdict(collided),
        AccountVerdict::Collision {
            salts: vec![CollidingSalt {
                salt_id: "salt-mbp".to_string(),
                accounts: 2,
            }],
        },
        "one salt, two values: the provable collision"
    );
}
