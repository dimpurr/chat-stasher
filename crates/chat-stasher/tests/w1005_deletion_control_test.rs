//! W1005 — ADR-046 draft Phase 0/1: the deletion control stream, hide/unhide,
//! and the purge dry-run preview.
//!
//! Properties pinned here:
//!
//! · `hide` appends one durable record to this machine's own
//!   `meta/<machine>/deletion-control-v1.jsonl` (ADR-017 one-writer) — never
//!   another machine's metadata partition.
//! · After the control stream is pushed, cumulative readers omit the hidden
//!   identity: `read --all-machines`, in-process `search_sessions` (the seam
//!   `search`, `export` and the UI share), and `read --session` refuses it as
//!   a finished-read policy refusal (exit 1, not the family-3 "did not
//!   finish").
//! · `unhide` restores visibility; the raw archive bytes of the session never
//!   change for either state (verify L3 still proves the stage against the
//!   archive, so "hidden" was presentation, not deletion).
//! · Partition isolation: hiding one machine's identity leaves the same
//!   session id under another machine partition visible (W402's exact-identity
//!   rule).
//! · `purge` previews read-only: per destination it counts every occurrence
//!   in every snapshot, exits 0 with complete reads, spells an unreadable
//!   destination `unknown` (exit 3), and **changes no byte** of any repo,
//!   destination or stage — asserted by hashing the whole destination
//!   directory and the whole stage before and after.
//! · `--confirm` is refused before any destination is read, because the
//!   destructive transaction does not exist in this build; `redact` refuses
//!   as a not-implemented surface. Both leave the world untouched.
//!
//! Fixture privacy: every session id and machine name below is a reserved
//! fixture identity (`w1005-…`, `chatgpt.synthetic-session`), the whole
//! sandbox lives under a process temp root, and the suites assert exit codes
//! and counts — never conversation text. `chatgpt.synthetic-session` is the
//! exact incident identity that the W306 namespace reserved, which is also
//! the first-real-use target of the purge design; using it here exercises
//! the identity guard's temp-root allowance, not an exception to it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chat_stasher::search::{search_sessions, SearchReport};
use chat_stasher::selector::Selector;
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};

#[path = "../src/test_support.rs"]
mod test_support;

use test_support::Sandbox;

const MACHINE: &str = "w1005-machine-one";
const OTHER_MACHINE: &str = "w1005-machine-two";
const SESSION: &str = "chatgpt.synthetic-session";

/// A store config for one local directory destination, cache pinned inside
/// the sandbox (the W289 rule: a repository may be opened with caching, only
/// never into the machine's real cache root).
fn store_config(repo: &Path, key: &Path, cache: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.display().to_string(),
        key_file: key.to_path_buf(),
        connections: 2,
        options: BTreeMap::new(),
        cache_dir: Some(cache.join("rustic-cache")),
        no_cache: false,
    }
}

/// One sealed synthetic session in a stage of its own — two shards, so
/// per-snapshot shard counts have something to count.
fn stage_with(root: &Path, machine: &str, session: &str, shards: usize) -> PathBuf {
    let stage = root.join(format!("stage-{}", machine));
    for n in 1..=shards {
        store::write_sealed_shard(
            StageWriter::Collect,
            &stage,
            machine,
            session,
            &[format!("{session} synthetic shard {n}")],
        )
        .expect("write the synthetic stage shard");
    }
    stage
}

/// No harness is installed in the sandbox: every assertion below is about the
/// control stream and the purge preview, so the registry must be empty.
fn empty_registry(root: &Path) -> PathBuf {
    let path = root.join("registry.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"generated":"W1005 synthetic","harnesses":[]}"#,
    )
    .expect("write the synthetic harness registry");
    path
}

/// One child CLI run with the whole sandbox environment pinned: home, config,
/// data, state and cache roots, the rustic-cache pin, and the registry.
fn command(sandbox: &Sandbox, registry: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    sandbox.apply(&mut command);
    command.env("CHAT_STASHER_REGISTRY", registry);
    command
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A config that declares two local destinations, so the named-destination
/// rules (`--destination`, `--all-destinations`) are exercised against a
/// multi-destination config rather than the legacy single mode.
fn write_two_destination_config(root: &Path, first: (&str, &Path), second: (&str, &Path)) {
    let path = root.join("config/chat-stasher/config.toml");
    fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
    let repo = |dir: &Path| dir.display().to_string();
    let key = |dir: &Path| dir.join("key.json").display().to_string();
    let cache = root.join("cache").display().to_string();
    fs::write(
        &path,
        format!(
            "[destinations.{name1}]\nrepo = \"{repo1}\"\nkey_file = \"{key1}\"\ncache_dir = \"{cache}\"\n\
             [destinations.{name2}]\nrepo = \"{repo2}\"\nkey_file = \"{key2}\"\ncache_dir = \"{cache}\"\n",
            name1 = first.0,
            repo1 = repo(first.1),
            key1 = key(first.1),
            name2 = second.0,
            repo2 = repo(second.1),
            key2 = key(second.1),
            cache = cache,
        ),
    )
    .expect("write the synthetic config");
}

/// Push the stage to a destination by name (multi-destination config mode).
fn push_to(sandbox: &Sandbox, registry: &Path, stage: &Path, machine: &str, name: &str) -> Output {
    let mut command = command(sandbox, registry);
    command.args(["push", "--stage"]).arg(stage).args([
        "--machine",
        machine,
        "--destination",
        name,
    ]);
    command.output().expect("run push")
}

/// A flat content hash of a whole directory tree: sorted relative paths and
/// file bytes. The purge preview must leave a destination and its stage
/// byte-identical, and this is the honest way to say so.
fn tree_fingerprint(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("read destination directory")
            .map(|entry| entry.expect("directory entry"))
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let bytes = fs::read(&path).expect("read destination file");
                out.push((
                    path.strip_prefix(root).expect("relative").to_path_buf(),
                    bytes,
                ));
            }
        }
    }
    let mut out = Vec::new();
    if root.exists() {
        walk(root, &root, &mut out);
    }
    out
}

/// In-process cumulative search over one destination, asserting the search
/// seam (`search`, `export`, the UI) sees exactly the ids named.
fn archive_sessions(cfg: &StoreConfig, expected_machine: &str) -> Vec<String> {
    let mk = store::load_key_file(cfg).expect("load the destination key");
    let store = BackupStore::for_metadata_query(cfg.clone());
    let report: SearchReport =
        search_sessions(&store, &mk, &Selector::default()).expect("search the archive");
    report
        .hits
        .iter()
        .filter(|hit| hit.machine == expected_machine)
        .map(|hit| hit.session_id.clone())
        .collect()
}

/// The must-fail checks: verify L3 still proves the stage against the
/// archive while a session is hidden, so hide is presentation, not deletion
/// (the raw shards stay archived and still match the staged bytes).
fn verify_l3(sandbox: &Sandbox, registry: &Path, stage: &Path, destination: &str) -> bool {
    let mut command = command(sandbox, registry);
    command
        .args(["verify", "--level", "l3", "--stage"])
        .arg(stage)
        .args(["--machine", MACHINE, "--destination", destination]);
    let output = command.output().expect("run verify");
    let (out, err) = text(&output);
    if !(out.contains("RESULT") && out.contains(": OK")) {
        eprintln!("verify stdout: {out}\nverify stderr: {err}");
    }
    out.contains("RESULT") && out.contains(": OK")
}

/// Hide → push → hidden from every reader, and the archive bytes untouched
/// (L3 still proves the stage). Unhide → push → back.
#[test]
fn hide_is_honored_by_readers_and_reversed_by_unhide() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 2);

    // Create the first destination's repository with an initial push (push
    // in single mode is not available with a multi-destination config, so the
    // named-mode push below both creates and archives).
    let first = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    let (out, err) = text(&first);
    assert!(first.status.success(), "initial push failed:\n{out}\n{err}");

    let cfg_a = store_config(
        &root.join("repos").join("a"),
        &root.join("repos").join("a").join("key.json"),
        &root.join("cache"),
    );

    // Visible before any control state exists.
    let mut read = command(&sandbox, &registry);
    read.args([
        "read",
        "--all-machines",
        "--full-ids",
        "--destination",
        "box-a",
    ]);
    let output = read.output().expect("run read");
    let (out, err) = text(&output);
    assert!(output.status.success(), "read failed:\n{out}\n{err}");
    assert!(
        out.contains(SESSION),
        "the session must be listed before hiding:\n{out}"
    );

    assert_eq!(
        archive_sessions(&cfg_a, MACHINE),
        vec![SESSION.to_string()],
        "search must see the session before hiding"
    );
    assert!(
        verify_l3(&sandbox, &registry, &stage, "box-a"),
        "verify L3 must pass before hiding"
    );

    // Original archive fingerprint, taken once visibility is established.
    let archive_before = tree_fingerprint(&root.join("repos").join("a"));

    // Hide, then push so the control stream rides the stage into snapshots.
    let mut hide = command(&sandbox, &registry);
    hide.args(["hide", "--stage"])
        .arg(&stage)
        .arg(SESSION)
        .args(["--machine", MACHINE]);
    let output = hide.output().expect("run hide");
    let (out, err) = text(&output);
    assert!(output.status.success(), "hide failed:\n{out}\n{err}");
    let control_file = stage
        .join("meta")
        .join(MACHINE)
        .join("deletion-control-v1.jsonl");
    let control = fs::read_to_string(&control_file).expect("read the control stream");
    assert_eq!(
        control.lines().count(),
        1,
        "hide appends exactly one record:\n{control}"
    );
    assert!(control.contains("\"action\":\"hide\""), "{control}");
    assert!(control.contains(SESSION), "{control}");

    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    let (out, err) = text(&pushed);
    assert!(
        pushed.status.success(),
        "push after hide failed:\n{out}\n{err}"
    );

    // Cumulative readers omit the identity.
    let mut read = command(&sandbox, &registry);
    read.args([
        "read",
        "--all-machines",
        "--full-ids",
        "--destination",
        "box-a",
    ]);
    let output = read.output().expect("run read after hide");
    let (out, err) = text(&output);
    assert!(
        output.status.success(),
        "read after hide failed:\n{out}\n{err}"
    );
    assert!(
        !out.contains(SESSION),
        "the hidden session must be omitted from `read --all-machines`:\n{out}"
    );

    assert_eq!(
        archive_sessions(&cfg_a, MACHINE),
        Vec::<String>::new(),
        "search must omit the hidden identity"
    );

    // The raw archive bytes of the session were never touched (W402: hide is
    // reversible presentation state). L3 still proves the stage against
    // them, and — the cross-check that matters — the purge preview, which
    // counts raw occurrences without applying any control state, still
    // finds every shard that existed before the hide.
    assert!(
        verify_l3(&sandbox, &registry, &stage, "box-a"),
        "verify L3 must still pass while the session is hidden: the shards are archived, only \
         presentation omitted them"
    );
    let mut preview = command(&sandbox, &registry);
    preview.arg("purge").arg(SESSION).args([
        "--machine",
        MACHINE,
        "--destination",
        "box-a",
        "--json",
    ]);
    let output = preview
        .output()
        .expect("run the preview over the hidden session");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}\n{err}");
    let document: serde_json::Value =
        serde_json::from_str(out.trim()).expect("the preview prints one JSON object");
    assert_eq!(document["destinations"][0]["status"], "found", "{document}");
    assert!(
        document["destinations"][0]["shard_occurrences"]["count"]
            .as_u64()
            .unwrap_or_default()
            >= 2,
        "hide must not delete; the physical occurrences are all still archived: {document}"
    );
    let _ = archive_before;

    // An explicit read of the hidden identity is a finished-read refusal:
    // exit 1, the family the control policy owns — never 3, which would say
    // the archive could not be read.
    let mut read_one = command(&sandbox, &registry);
    read_one
        .args(["read", "--destination", "box-a", "--machine", MACHINE])
        .arg(format!("--session={SESSION}"));
    let output = read_one.output().expect("run read --session after hide");
    let (out, err) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a hidden session read refusal must exit 1 (finished read, policy refusal), not 3 or 0:\n{out}\n{err}"
    );
    assert!(
        err.contains("unhide"),
        "the refusal must say how to restore:\n{err}"
    );

    // Idempotency: hiding again appends nothing.
    let mut hide_again = command(&sandbox, &registry);
    hide_again
        .args(["hide", "--stage"])
        .arg(&stage)
        .arg(SESSION)
        .args(["--machine", MACHINE]);
    let output = hide_again.output().expect("run hide again");
    let (out, err) = text(&output);
    assert!(
        output.status.success(),
        "second hide must succeed:\n{out}\n{err}"
    );
    assert!(
        out.contains("not appended"),
        "the already-hidden state must say no record was appended:\n{out}"
    );
    assert_eq!(
        fs::read_to_string(&control_file)
            .expect("re-read the control stream")
            .lines()
            .count(),
        1,
        "the idempotent hide must not append a second record"
    );

    // Unhide restores visibility through the same readers.
    let mut unhide = command(&sandbox, &registry);
    unhide
        .args(["unhide", "--stage"])
        .arg(&stage)
        .arg(SESSION)
        .args(["--machine", MACHINE]);
    let output = unhide.output().expect("run unhide");
    let (out, err) = text(&output);
    assert!(output.status.success(), "unhide failed:\n{out}\n{err}");
    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    let (out, err) = text(&pushed);
    assert!(
        pushed.status.success(),
        "push after unhide failed:\n{out}\n{err}"
    );

    let mut read = command(&sandbox, &registry);
    read.args([
        "read",
        "--all-machines",
        "--full-ids",
        "--destination",
        "box-a",
    ]);
    let output = read.output().expect("run read after unhide");
    let (out, err) = text(&output);
    assert!(
        output.status.success(),
        "read after unhide failed:\n{out}\n{err}"
    );
    assert!(
        out.contains(SESSION),
        "unhide must restore the listing:\n{out}"
    );
    assert_eq!(
        archive_sessions(&cfg_a, MACHINE),
        vec![SESSION.to_string()],
        "search must see the session again after unhide"
    );
}

/// The W402 exact-identity rule, end to end: the same session id under two
/// machine partitions is two identities, and hiding one leaves the other
/// visible. The control record also stays in the hider's own metadata
/// partition (ADR-017), even though the target partition is the other
/// machine's.
#[test]
fn hide_targets_one_partition_and_writes_the_actors_own_meta() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    // One stage per machine, one session id each — the identity differs only
    // by partition, which is exactly the confusion W402 forbids.
    let stage_a = stage_with(root, MACHINE, SESSION, 1);
    let stage_b = stage_with(root, OTHER_MACHINE, SESSION, 1);
    for (stage, machine) in [(&stage_a, MACHINE), (&stage_b, OTHER_MACHINE)] {
        let pushed = push_to(&sandbox, &registry, stage, machine, "box-a");
        let (out, err) = text(&pushed);
        assert!(
            pushed.status.success(),
            "push for {machine} failed:\n{out}\n{err}"
        );
    }

    // Hide OTHER_MACHINE's identity, recorded by MACHINE's own writer
    // partition in MACHINE's own stage.
    let mut hide = command(&sandbox, &registry);
    hide.args(["hide", "--stage"])
        .arg(&stage_a)
        .arg(SESSION)
        .args(["--machine", MACHINE])
        .args(["--target-machine", OTHER_MACHINE]);
    let output = hide.output().expect("run cross-partition hide");
    let (out, err) = text(&output);
    assert!(
        output.status.success(),
        "cross-partition hide failed:\n{out}\n{err}"
    );
    let control = fs::read_to_string(
        stage_a
            .join("meta")
            .join(MACHINE)
            .join("deletion-control-v1.jsonl"),
    )
    .expect("the record must live in the actor's own partition");
    assert!(
        fs::read_to_string(
            stage_a
                .join("meta")
                .join(OTHER_MACHINE)
                .join("deletion-control-v1.jsonl"),
        )
        .is_err(),
        "no control stream may exist in the target machine's partition on this stage (ADR-017)"
    );
    assert!(
        control.contains(&format!("\"target_machine\":\"{OTHER_MACHINE}\"")),
        "the record names the target partition:\n{control}"
    );

    // Push both stages so the readers see the control union.
    for (stage, machine) in [(&stage_a, MACHINE), (&stage_b, OTHER_MACHINE)] {
        let pushed = push_to(&sandbox, &registry, stage, machine, "box-a");
        assert!(
            pushed.status.success(),
            "push after cross-partition hide failed for {machine}"
        );
    }

    let mut read = command(&sandbox, &registry);
    read.args([
        "read",
        "--all-machines",
        "--full-ids",
        "--destination",
        "box-a",
    ]);
    let output = read.output().expect("run read after cross-partition hide");
    let (out, err) = text(&output);
    assert!(output.status.success(), "read failed:\n{out}\n{err}");
    assert!(
        out.contains(SESSION) || out.contains(&format!("{MACHINE}/{SESSION}")),
        "the visible partition's copy must stay listed:\n{out}"
    );
    assert!(
        out.contains(&format!("machine {OTHER_MACHINE}")),
        "the other machine must still be listed as a machine:\n{out}"
    );
    // The machine section rows: the hidden identity must be gone from the
    // other machine's session list while the same id stays visible under the
    // hiding record was about. Extract the other machine's block.
    let block = out
        .split(&format!("machine {OTHER_MACHINE}"))
        .nth(1)
        .expect("the other machine is listed");
    let block = block.split("machine ").next().expect("block bounded");
    assert!(
        !block.contains(SESSION),
        "the hidden partition's copy of the id must be omitted:\n{block}"
    );
    let visible_block = out
        .split(&format!("machine {MACHINE}"))
        .nth(1)
        .expect("the hider machine is listed");
    let visible_block = visible_block
        .split("machine ")
        .next()
        .expect("block bounded");
    assert!(
        visible_block.contains(SESSION),
        "the untouched partition's same-id session must remain visible (exact-identity rule):\n{visible_block}"
    );
}

/// The purge preview: per-destination counts across every snapshot, honest
/// statuses, and not one byte written anywhere.
#[test]
fn purge_preview_inventories_every_destination_and_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 2);

    // Push each destination twice so the session is provably present in two
    // snapshots per destination, then push once more with a new shard so a
    // third snapshot holds three shard files and the preview must count
    // occurrences in *every* snapshot, cumulatively (W402).
    for destination in ["box-a", "box-b"] {
        for round in 0..2 {
            let pushed = push_to(&sandbox, &registry, &stage, MACHINE, destination);
            let (out, err) = text(&pushed);
            assert!(
                pushed.status.success(),
                "push {round} to {destination} failed:\n{out}\n{err}"
            );
        }
    }
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        MACHINE,
        SESSION,
        &["chatgpt.synthetic-session synthetic shard 3".to_string()],
    )
    .expect("add a third shard");
    for destination in ["box-a", "box-b"] {
        let pushed = push_to(&sandbox, &registry, &stage, MACHINE, destination);
        assert!(
            pushed.status.success(),
            "third push to {destination} failed"
        );
    }

    let fingerprints_before: Vec<_> = ["a", "b"]
        .iter()
        .map(|tag| tree_fingerprint(&root.join("repos").join(tag)))
        .collect();
    let stage_before = tree_fingerprint(&stage);

    // Dry run over every declared destination, machine-readable.
    let mut purge = command(&sandbox, &registry);
    purge
        .arg("purge")
        .arg(SESSION)
        .args(["--machine", MACHINE, "--all-destinations", "--json"]);
    let output = purge.output().expect("run the purge preview");
    let (out, err) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a complete preview with present occurrences must exit 0:\n{out}\n{err}"
    );
    let document: serde_json::Value = serde_json::from_str(out.trim())
        .expect("the preview must print exactly one JSON object on stdout");
    assert_eq!(document["summary"], "would-purge", "{document}");
    assert_eq!(document["session_id"], SESSION, "{document}");
    assert_eq!(document["machine_partition"], MACHINE, "{document}");
    assert_eq!(document["mode"], "dry-run", "{document}");
    let destinations = document["destinations"]
        .as_array()
        .expect("destinations array");
    assert_eq!(destinations.len(), 2, "{document}");
    for destination in destinations {
        assert_eq!(destination["status"], "found", "{destination}");
        assert_eq!(destination["snapshots_with_target"], 3, "{destination}");
        // Three snapshots hold the target cumulatively: 2 + 2 + 3 shard
        // files — every occurrence counted (W402), never the newest
        // snapshot alone.
        assert_eq!(
            destination["shard_occurrences"]["count"], 7,
            "{destination}"
        );
        let bytes = destination["bytes"]["kind"] == "known"
            && destination["bytes"]["count"].as_u64().unwrap_or_default() > 0;
        assert!(
            bytes,
            "metadata-tier byte counts must be present: {destination}"
        );
    }

    // Narrow selection answers for exactly the destination named.
    let mut narrow = command(&sandbox, &registry);
    narrow.arg("purge").arg(SESSION).args([
        "--machine",
        MACHINE,
        "--destination",
        "box-a",
        "--json",
    ]);
    let output = narrow.output().expect("run the narrowed preview");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}\n{err}");
    let document: serde_json::Value =
        serde_json::from_str(out.trim()).expect("the narrowed preview prints one JSON object");
    assert_eq!(
        document["destinations"]
            .as_array()
            .expect("destinations array")
            .len(),
        1,
        "{document}"
    );

    // An unreadable destination spelling is covered by its own test below
    // (a config that points a destination at a missing repository), so this
    // run proceeds with the human-mode checks.

    // Human mode reports the W402 statuses worded apart.
    let mut purge = command(&sandbox, &registry);
    purge
        .arg("purge")
        .arg(SESSION)
        .args(["--machine", MACHINE, "--all-destinations"]);
    let output = purge.output().expect("run the human preview");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}\n{err}");
    assert!(
        out.contains("status=found"),
        "every destination is found and must say so in W402's words:\n{out}"
    );
    assert!(out.contains("WOULD-PURGE"), "{out}");
    assert!(out.contains("dry-run"), "{out}");

    // The zero-write property, in bytes: destinations and stage are
    // byte-identical after all preview runs.
    for (index, tag) in ["a", "b"].iter().enumerate() {
        assert_eq!(
            tree_fingerprint(&root.join("repos").join(tag)),
            fingerprints_before[index],
            "the preview must not change a byte of destination {tag}"
        );
    }
    assert_eq!(
        tree_fingerprint(&stage),
        stage_before,
        "the preview must not touch the staged copy (W402: the stage is not a purge target)"
    );
}

/// An unreadable destination is `unknown`, never `absent`; the exit is the
/// did-not-finish family; and one working destination's answer is still
/// reported beside it.
#[test]
fn purge_preview_unknown_destination_is_never_absent() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("missing")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 1);
    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    let (out, err) = text(&pushed);
    assert!(
        pushed.status.success(),
        "push to box-a failed:\n{out}\n{err}"
    );

    let mut purge = command(&sandbox, &registry);
    purge
        .arg("purge")
        .arg(SESSION)
        .args(["--machine", MACHINE, "--all-destinations", "--json"]);
    let output = purge
        .output()
        .expect("run the preview with a broken destination");
    let (out, err) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "an unknown destination must make the whole preview exit 3:\n{out}\n{err}"
    );
    let document: serde_json::Value = serde_json::from_str(out.trim())
        .expect("the refusing preview still prints one JSON object");
    assert_eq!(document["summary"], "unknown", "{document}");
    assert_eq!(document["unknown_destinations"], 1, "{document}");
    let destinations = document["destinations"].as_array().expect("array");
    let statuses: Vec<&str> = destinations
        .iter()
        .map(|destination| destination["status"].as_str().expect("status"))
        .collect();
    assert!(statuses.contains(&"unknown"), "{document}");
    assert!(
        statuses.contains(&"found"),
        "the reachable destination is still reported beside the unknown one: {document}"
    );

    let mut purge = command(&sandbox, &registry);
    purge
        .arg("purge")
        .arg(SESSION)
        .args(["--machine", MACHINE, "--all-destinations"]);
    let output = purge.output().expect("run the human preview");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}\n{err}");
    assert!(
        out.contains("status=unknown") && out.contains("not read to completion"),
        "the human report must keep unknown and absent worded apart:\n{out}"
    );
}

/// A proven absence is exit 0 and the word "absent" — proved by a complete
/// read, not assumed from a short scan.
#[test]
fn purge_preview_absent_requires_a_complete_read() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 1);
    for destination in ["box-a", "box-b"] {
        let pushed = push_to(&sandbox, &registry, &stage, MACHINE, destination);
        assert!(
            pushed.status.success(),
            "push to {destination} failed: {}",
            text(&pushed).1
        );
    }

    // Purge an identity no snapshot holds. The read is complete, so the
    // absence is proved — exit 0, status absent, never "unknown".
    let mut purge = command(&sandbox, &registry);
    purge.arg("purge").arg("chatgpt.absent-session").args([
        "--machine",
        MACHINE,
        "--all-destinations",
        "--json",
    ]);
    let output = purge.output().expect("run the absent preview");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}\n{err}");
    let document: serde_json::Value =
        serde_json::from_str(out.trim()).expect("the absent preview prints one JSON object");
    assert_eq!(document["summary"], "absent", "{document}");
    for destination in document["destinations"].as_array().expect("array") {
        assert_eq!(destination["status"], "absent", "{destination}");
        assert_eq!(
            destination["shard_occurrences"]["count"], 0,
            "{destination}"
        );
    }
}

/// The destructive flags are refused before anything is read: `--confirm`
/// names a phase that does not exist, a misnamed confirmation is a usage
/// error, `redact` is a not-implemented refusal, and bare `purge` without a
/// destination selection is a usage error too.
#[test]
fn destructive_flags_are_refused_and_nothing_is_read_or_changed() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 1);
    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    assert!(
        pushed.status.success(),
        "push to box-a failed: {}",
        text(&pushed).1
    );
    let before = tree_fingerprint(&root.join("repos").join("a"));

    // A correctly named confirmation is still refused: this build has no
    // destructive mode, and the refusal happens before the destinations are
    // read.
    let mut confirm = command(&sandbox, &registry);
    confirm
        .arg("purge")
        .arg(SESSION)
        .args(["--machine", MACHINE, "--all-destinations"])
        .arg(format!("--confirm={SESSION}"));
    let output = confirm.output().expect("run purge --confirm");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(3), "{out}\n{err}");
    assert!(err.contains("--confirm is refused"), "{err}");
    assert!(
        err.contains("nothing was read or changed"),
        "the refusal must state the zero-effect contract:\n{err}"
    );

    // A misnamed confirmation is a usage error.
    let mut wrong = command(&sandbox, &registry);
    wrong.arg("purge").arg(SESSION).args([
        "--machine",
        MACHINE,
        "--all-destinations",
        "--confirm=other-session",
    ]);
    let output = wrong.output().expect("run purge --confirm other");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // No destination selection is a usage error: there is no implicit set.
    let mut bare = command(&sandbox, &registry);
    bare.arg("purge").arg(SESSION).args(["--machine", MACHINE]);
    let output = bare.output().expect("run bare purge");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // Both selection modes at once is a usage error.
    let mut both = command(&sandbox, &registry);
    both.arg("purge").arg(SESSION).args([
        "--machine",
        MACHINE,
        "--all-destinations",
        "--destination",
        "box-a",
    ]);
    let output = both.output().expect("run purge with both selections");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // redact exists on the surface and refuses: not-implemented, exit 2.
    let mut redact = command(&sandbox, &registry);
    redact
        .arg("redact")
        .arg(SESSION)
        .args(["--machine", MACHINE]);
    let output = redact.output().expect("run redact");
    let (out, err) = text(&output);
    assert_eq!(output.status.code(), Some(2), "{out}\n{err}");
    assert!(err.contains("not implemented"), "{err}");

    // Nothing anywhere changed across every refusal.
    assert_eq!(
        tree_fingerprint(&root.join("repos").join("a")),
        before,
        "the refused destructive paths must leave the destination untouched"
    );
}

/// Usage guards of the state commands: a missing stage, an invalid session
/// id, an over-long reason label, and a refusal to append over an
/// unreadable control stream.
#[test]
fn hide_usage_guards_fail_closed() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    let stage = stage_with(root, MACHINE, SESSION, 1);

    // Missing --stage directory.
    let mut missing = command(&sandbox, &registry);
    missing
        .arg("hide")
        .arg(SESSION)
        .args(["--machine", MACHINE])
        .arg("--stage")
        .arg(&sandbox.root().join("no-such-stage"));
    let output = missing.output().expect("run hide with a missing stage");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // Inavlid session id spelling.
    let mut invalid = command(&sandbox, &registry);
    invalid
        .arg("hide")
        .arg("bad/id")
        .args(["--machine", MACHINE])
        .arg("--stage")
        .arg(&stage);
    let output = invalid.output().expect("run hide with an invalid id");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // Over-long reason label.
    let mut long_reason = command(&sandbox, &registry);
    long_reason
        .arg("hide")
        .arg(SESSION)
        .args(["--machine", MACHINE])
        .arg("--stage")
        .arg(&stage)
        .arg("--reason")
        .arg("r".repeat(200));
    let output = long_reason
        .output()
        .expect("run hide with an over-long reason");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output).1);

    // Unreadable control stream: append refused before touching it.
    let control_dir = stage.join("meta").join(MACHINE);
    fs::create_dir_all(&control_dir).expect("create the meta partition");
    fs::write(control_dir.join("deletion-control-v1.jsonl"), "{not-json\n")
        .expect("plant an unreadable control stream");
    let mut over_unreadable = command(&sandbox, &registry);
    over_unreadable
        .arg("hide")
        .arg(SESSION)
        .args(["--machine", MACHINE])
        .arg("--stage")
        .arg(&stage);
    let output = over_unreadable
        .output()
        .expect("run hide over an unreadable stream");
    let (out, err) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
    "an unreadable control stream is unknown, and unknown is never \"nothing recorded\":\n{out}\n{err}"
    );
    assert!(
        fs::read_to_string(control_dir.join("deletion-control-v1.jsonl"))
            .expect("re-read the stream")
            == "{not-json\n",
        "the refused append must leave the unreadable stream exactly as it was"
    );
}

/// The reader fail-closed contract: an archived control stream that cannot
/// be parsed must refuse the content-bearing reads, not fold an empty policy
/// over it. Planted by hand into the stage — the same record bytes a real
/// push would archive, but corrupt, which is the state a torn or tampered
/// stream is.
#[test]
fn a_malformed_archived_control_stream_fails_reads_closed() {
    let sandbox = Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let registry = empty_registry(root);
    write_two_destination_config(
        root,
        ("box-a", &root.join("repos").join("a")),
        ("box-b", &root.join("repos").join("b")),
    );
    let stage = stage_with(root, MACHINE, SESSION, 1);
    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    assert!(
        pushed.status.success(),
        "push to box-a failed: {}",
        text(&pushed).1
    );
    let control_dir = stage.join("meta").join(MACHINE);
    fs::create_dir_all(&control_dir).expect("create the meta partition");
    fs::write(
        control_dir.join("deletion-control-v1.jsonl"),
        "{torn-record\n",
    )
    .expect("plant a torn control record");
    let pushed = push_to(&sandbox, &registry, &stage, MACHINE, "box-a");
    assert!(
        pushed.status.success(),
        "push with the torn control record failed: {}",
        text(&pushed).1
    );

    // `read --all-machines` and the search seam must refuse (exit 3), never
    // report an inventory over an unknown control state.
    let mut read = command(&sandbox, &registry);
    read.args([
        "read",
        "--all-machines",
        "--full-ids",
        "--destination",
        "box-a",
    ]);
    let output = read.output().expect("run read over the torn stream");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a torn control stream must fail the read closed:\n{}",
        text(&output).0
    );

    let cfg_a = store_config(
        &root.join("repos").join("a"),
        &root.join("repos").join("a").join("key.json"),
        &root.join("cache"),
    );
    let mk = store::load_key_file(&cfg_a).expect("load the destination key");
    let store = BackupStore::for_metadata_query(cfg_a.clone());
    let search = search_sessions(&store, &mk, &Selector::default());
    assert!(
        search.is_err(),
        "the search seam must refuse to fold a torn control stream into a policy"
    );

    // Verify that the torn stream's bytes are themselves unchanged: the
    // refused readers wrote nothing anywhere.
    assert!(
        fs::read_to_string(control_dir.join("deletion-control-v1.jsonl"))
            .expect("re-read the stage stream")
            == "{torn-record\n",
        "no reader may repair or remove a torn control stream"
    );
}
