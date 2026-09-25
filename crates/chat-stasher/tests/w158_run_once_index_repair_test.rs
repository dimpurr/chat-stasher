//! W158 — a pass that pushes nothing must still repair *this machine's*
//! archived activity index when the archive says it was written by an older
//! chat-stasher.
//!
//! The index and the writer version are written by the same binary into the
//! same snapshot, so "writer version behind the running one" means the archived
//! index is that older build's reading — and `overview`, `search` and `ui` all
//! read the archive, not the stage. Before this, a machine that had nothing new
//! to collect sat on the old index until somebody ran `activity-index
//! --rebuild` by hand, which is exactly the i7 report this item came from.
//!
//! The archive here is built with `BackupStore::push` directly, which records
//! no writer version — the ≤0.3.0 shape `overview` renders as
//! `behind (version not recorded — written by ≤0.3.0)`. A second machine's
//! partition is archived the same way and must be left untouched: rebuilding
//! another machine's partition would replay shards while that machine may be
//! pushing them (ADR-016: no cross-process lock).

use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use rustic_core::{Credentials, Repository};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W158 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
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

fn cfg(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

/// How many snapshots the repository holds per hostname.
fn snapshots_per_host(cfg: &StoreConfig, mk: &MasterKey) -> BTreeMap<String, usize> {
    let backends = BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .unwrap();
    let repo = Repository::new(&cfg.repository_options(), &backends)
        .unwrap()
        .open(&Credentials::Masterkey(mk.clone()))
        .unwrap()
        .to_indexed()
        .unwrap();
    let mut counts = BTreeMap::new();
    for snapshot in repo.get_all_snapshots().unwrap() {
        *counts.entry(snapshot.hostname.clone()).or_insert(0) += 1;
    }
    counts
}

#[test]
fn run_once_repairs_a_stale_archived_index_and_leaves_other_machines_alone() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let machine_a = "mbp-a";
    let machine_b = "mbp-b";
    let stage_a = sandbox.join("stage-a");
    let stage_b = sandbox.join("stage-b");

    write_shard(
        &stage_a,
        machine_a,
        "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );
    write_shard(
        &stage_b,
        machine_b,
        "claude-code.mbp-b.019bf00d-97b6-7eb2-9bf8-eacbacc09766",
        &[cc_line("2025-02-01T09:00:00Z")],
    );

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let cfg = cfg(&repo, &key);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    // Archived without a writer record: the ≤0.3.0 state.
    BackupStore::new(cfg.clone(), machine_a.to_string())
        .push(&stage_a, &mk)
        .unwrap();
    BackupStore::new(cfg.clone(), machine_b.to_string())
        .push(&stage_b, &mk)
        .unwrap();

    let before = snapshots_per_host(&cfg, &mk);
    assert_eq!(before.get(machine_a), Some(&1));
    assert_eq!(before.get(machine_b), Some(&1));

    // The archive's own answer before the pass, through the reader `overview`
    // uses: no writer version, i.e. behind whatever is running.
    let out = run(
        sandbox,
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let before_text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        before_text.contains("behind (version not recorded — written by ≤0.3.0)"),
        "the fixture must start out with no writer record:\n{before_text}"
    );

    // One pass that has nothing to push (no meta files, one sealed shard, no
    // collected change) — so it must NOT push, and must still repair the index.
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage_a.to_str().unwrap(),
            "--machine",
            machine_a,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let all = format!("{stdout}{stderr}");
    assert!(out.status.success(), "run-once should exit 0:\n{all}");
    assert!(
        stdout.contains("[run-once] push skipped:"),
        "this fixture must exercise the no-push path:\n{all}"
    );
    assert!(
        stdout.contains(&format!(
            "[run-once] activity-index: machine {machine_a} archived writer version=not recorded \
             (written by ≤0.3.0) running={}",
            env!("CARGO_PKG_VERSION")
        )),
        "the pass must name the machine, the recorded version and the running one:\n{all}"
    );
    assert!(
        stdout.contains("[run-once] activity-index: repaired snapshot appended"),
        "the pass must report the rebuild:\n{all}"
    );

    // Exactly one new snapshot, and only for this machine.
    let after = snapshots_per_host(&cfg, &mk);
    assert_eq!(
        after.get(machine_a),
        Some(&2),
        "the repair must append exactly one snapshot for this machine"
    );
    assert_eq!(
        after.get(machine_b),
        Some(&1),
        "another machine's partition must not be rebuilt"
    );

    // And the archive now answers with this build's version — the same read
    // `overview` prints, which is what the item was reported against.
    let out = run(
        sandbox,
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let after_text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        after_text.contains(&format!(
            "machine={machine_a} (unnamed) version={} behind-newest=no",
            env!("CARGO_PKG_VERSION")
        )),
        "this machine must now be reported at the running version:\n{after_text}"
    );
    assert!(
        after_text.contains(&format!("machine={machine_b} (unnamed) version=behind")),
        "the other machine must still be reported as having no writer record:\n{after_text}"
    );
}

/// The other half of the item: when the repair has *not* happened, `doctor` has
/// to say so and name the command that does it. Asserted both ways round —
/// through `--json` (the machine-readable field) and through the human report
/// (which is where a person reads it).
#[test]
fn doctor_names_the_exact_repair_command_for_a_stale_index() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let machine = "mbp-a";
    let stage = sandbox.join("stage");
    write_shard(
        &stage,
        machine,
        "claude-code.mbp-a.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        &[cc_line("2025-01-15T12:34:56.789Z")],
    );

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let cfg = cfg(&repo, &key);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(&stage, &mk)
        .unwrap();

    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.fixture]\nrepo = \"{}\"\nkey_file = \"{}\"\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();

    let expected = format!(
        "chat-stasher activity-index --rebuild --destination fixture --machine {machine} \
         --stage <workspace>"
    );

    let out = run(sandbox, &["doctor", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "doctor --json should exit 0:\n{stdout}"
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor --json prints one JSON object");
    let probe = &value["destinations"][0];
    assert_eq!(probe["name"], "fixture");
    assert_eq!(probe["activity_index"]["kind"], "behind");
    assert_eq!(probe["activity_index"]["stale"][0]["machine"], machine);
    assert_eq!(
        probe["activity_index"]["stale"][0]["recorded_version"],
        serde_json::Value::Null,
        "no writer record is null, not an empty version"
    );
    assert_eq!(
        probe["activity_index"]["stale"][0]["repair_command"],
        expected
    );

    let out = run(sandbox, &["doctor"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("activity index: ⚠ 1 of 1 machine(s)"),
        "the human report must name how many indexes are stale:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("rebuild it with: {expected}")),
        "the human report must print the exact command:\n{stderr}"
    );
}
