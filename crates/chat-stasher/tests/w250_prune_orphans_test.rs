//! W250 — `prune-orphans` reports the packs no index file names, and refuses to
//! delete any of them.
//!
//! Three properties are pinned here, all at the CLI boundary because that is
//! where an operator meets the command:
//!
//!   1. **It reads and reports.** A repository whose index and backend agree
//!      reports nothing stranded; a repository holding packs a killed push left
//!      behind reports each one with the size the backend listed, and says what
//!      the next push would do about it. A pack that cannot be read is reported
//!      **unknown** with the verifier's reason — never as empty, never as zero —
//!      and it makes the whole survey incomplete, which is exit `3`.
//!   2. **It never writes.** `--apply` is refused with exit `3`, naming the
//!      capabilities a safe delete would need, and the repository tree is
//!      byte-identical after every run — apply, dry run, and `--json`.
//!   3. **It leaks nothing.** The report carries the repository's own id, not
//!      the path it was reached at; no line contains a path, a host or a
//!      machine name.
//!
//! The fixture is the one `w245_interrupted_push_test.rs` drives: a real push
//! from a synthetic stage, then `strand_packs` — drop the index and the
//! snapshots, keep every pack — which is exactly the state a kill inside the
//! pack window leaves.
//!
//! Nothing here is a race: the stranded state is constructed directly rather
//! than caught, so the tests are deterministic on a loaded machine.

use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

/// A deterministic, poorly-compressible payload, so a push writes real packs
/// without the test needing a random-number dependency.
fn filler(bytes: usize, seed: u64) -> String {
    let mut state = seed | 1;
    let hex = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes);
    while out.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(hex[(state & 0xf) as usize] as char);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A sandbox: one stage with synthetic sessions, one local repository, one key.
struct Sandbox {
    dir: tempfile::TempDir,
    registry: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
}

impl Sandbox {
    fn new(sessions: usize, bytes_each: usize) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for sub in ["home", "config", "data", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W250 prune-orphans","harnesses":[]}"#,
        )
        .unwrap();

        let stage = root.join("stage");
        let machine = "w250-fixture";
        for i in 0..sessions {
            let session = format!("claude-code.{machine}.019bf00d-97b6-7eb2-9bf8-{i:012}");
            let body = format!(
                r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1","timestamp":"2025-06-01T10:00:00Z"}}"#,
                filler(bytes_each, 0x9E37_79B9_7F4A_7C15 ^ i as u64)
            ) + "\n";
            let dir = stage
                .join("sessions")
                .join(machine)
                .join(&session)
                .join("000");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("000001.jsonl"), &body).unwrap();
        }

        Sandbox {
            dir,
            registry,
            repo: root.join("repo"),
            key: root.join("key.json"),
            stage,
        }
    }

    fn command(&self) -> Command {
        let root = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&root.join("home")),
            )
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("CHAT_STASHER_REGISTRY", &self.registry);
        cmd
    }

    /// The `--repo`/`--key-file` pair that names this sandbox's destination
    /// without a config file.
    fn repo_args(&self) -> Vec<String> {
        vec![
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
        ]
    }

    fn store_config(&self) -> StoreConfig {
        StoreConfig {
            repo_root: self.repo.to_string_lossy().into_owned(),
            key_file: self.key.clone(),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            // A cached index or pack could answer for a repository this test has
            // just edited on disk, which is the one thing these tests change.
            no_cache: true,
        }
    }

    fn push(&self) -> Output {
        let mut args = vec![
            "push".to_string(),
            "--stage".to_string(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".to_string(),
            "w250-fixture".to_string(),
        ];
        args.extend(self.repo_args());
        args.push("--keep-ssh-masters".to_string());
        self.command().args(args).output().unwrap()
    }

    /// Create the repository and stop: config only, no packs and no index. The
    /// state a destination is in between `init` and its first push.
    fn init_only(&self) {
        let cfg = self.store_config();
        let mk = MasterKey::new();
        store::persist_key_file(&cfg, &mk).unwrap();
        BackupStore::new(cfg, "w250-fixture".to_string())
            .open_or_init(&mk)
            .unwrap();
    }

    /// Run the command with `--repo`/`--key-file`. `--keep-ssh-masters` is
    /// deliberately **not** passed: it makes the ssh reaper narrate on stdout
    /// (`[reap] skipped …`), and the `--json` contract is one object and nothing
    /// else there. With no `endpoint` option the reaper prints nothing anyway.
    fn prune(&self, extra: &[&str]) -> Output {
        let mut args = vec!["prune-orphans".to_string()];
        args.extend(self.repo_args());
        args.extend(extra.iter().map(|s| (*s).to_string()));
        self.command().args(args).output().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn unix_rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every file below `repo`, as `relative path -> (bytes, sha256 of contents)`.
///
/// Content, not just names: a run that rewrote a pack in place with different
/// bytes but the same length would pass a name-only comparison, and "the
/// repository is byte-identical" is the claim under test.
fn tree_bytes(repo: &Path) -> BTreeMap<String, (u64, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![repo.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                let key = unix_rel(&path, repo);
                out.insert(key, (bytes.len() as u64, sha256_hex(&bytes)));
            }
        }
    }
    out
}

/// A pack file name is the pack's own id: 32 bytes of hex, nothing else.
///
/// `-tmp-` matters: `LocalBackend` writes a pack to `data/<xx>/<id>-tmp-` and
/// renames it into place, so a file with that suffix is one nothing may treat as
/// a finished pack.
fn is_final_pack_name(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.len() == 64 && file.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every finished pack file below `repo`, sorted.
fn pack_paths(repo: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = tree_bytes(repo)
        .keys()
        .filter(|name| name.starts_with("data/") && is_final_pack_name(name))
        .map(|name| repo.join(name))
        .collect();
    packs.sort();
    packs
}

/// Where a pack's blob region ends and its header begins.
///
/// A pack is `blob, blob, …, header, header-length`: the header is written last
/// and the length of it — unencrypted, four bytes little-endian — after it. So
/// the last four bytes say how far back the header starts, which is what a test
/// needs to damage a blob while leaving the header and the file's length alone.
fn pack_blob_region(bytes: &[u8]) -> usize {
    let end = bytes.len();
    let header_len = u32::from_le_bytes([
        bytes[end - 4],
        bytes[end - 3],
        bytes[end - 2],
        bytes[end - 1],
    ]) as usize;
    end - 4 - header_len
}

/// Drop the index and the snapshots, keeping `config` and every pack: exactly
/// the state a kill inside the pack window leaves.
fn strand_packs(sb: &Sandbox) -> Vec<PathBuf> {
    let stranded = pack_paths(&sb.repo);
    assert!(!stranded.is_empty(), "the push wrote no packs to strand");
    for entry in ["index", "snapshots"] {
        let dir = sb.repo.join(entry);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
    }
    stranded
}

/// The line for one field of the human report, e.g. `[prune] packs       : 0`.
fn field(text: &str, name: &str) -> String {
    text.lines()
        .find(|line| line.starts_with(&format!("[prune] {name}")))
        .unwrap_or_else(|| panic!("no `[prune] {name}` line in:\n{text}"))
        .to_string()
}

/// A repository with no packs at all — the state between `init` and the first
/// push — reports zero, and zero is a measurement here, not a fallback.
#[test]
fn an_empty_repository_reports_zero_packs_and_completes() {
    let sb = Sandbox::new(1, 4096);
    sb.init_only();

    let out = sb.prune(&[]);
    let text = stdout(&out);

    assert!(
        out.status.success(),
        "an empty repository is a clean survey, not a failure: {text}{}",
        stderr(&out)
    );
    assert!(
        field(&text, "packs").contains("0 ("),
        "the empty listing must be reported as a count: {text}"
    );
    assert!(
        text.contains("nothing to adopt"),
        "with no packs there is nothing stranded: {text}"
    );
    assert!(
        text.contains("RESULT      : COMPLETE"),
        "reading an empty repository finishes: {text}"
    );
}

/// A repository whose index names every pack in its backend is the healthy one,
/// and it must report exactly that rather than an absence.
#[test]
fn a_healthy_repository_reports_nothing_stranded() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));

    let out = sb.prune(&[]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(
        field(&text, "unindexed").contains(": 0 ("),
        "a repository whose index and backend agree strands nothing: {text}"
    );
    assert!(
        text.contains("nothing to adopt"),
        "the next push has nothing to adopt here: {text}"
    );
}

/// The case the command exists for: packs a killed push left behind, each one
/// verified and adoptable, and the report says so.
#[test]
fn stranded_packs_are_reported_verified_and_adoptable() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    let stranded = strand_packs(&sb);

    let before = tree_bytes(&sb.repo);
    let out = sb.prune(&[]);
    let text = stdout(&out);
    let after = tree_bytes(&sb.repo);

    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(
        field(&text, "unindexed").contains(&format!(": {} (", stranded.len())),
        "every stranded pack is reported: {text}"
    );
    assert!(
        text.contains("0 unknown (0 B)"),
        "a verified pack is never counted as unknown: {text}"
    );
    assert!(
        text.contains(&format!("would adopt all {}", stranded.len())),
        "the next push adopts the whole set: {text}"
    );
    assert_eq!(before, after, "a dry run must not touch the repository");
}

/// A pack whose bytes no longer read back as the id it is stored under is
/// **unknown**, carries the verifier's reason, and makes the survey
/// incomplete — exit `3`, never a clean empty result.
#[test]
fn a_damaged_orphan_is_reported_unknown_and_makes_the_survey_incomplete() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    let stranded = strand_packs(&sb);

    // Flip one byte inside the blob region, leaving the name and the length
    // alone: damage under the old name, which only reading the bytes catches.
    let victim = &stranded[0];
    let mut bytes = fs::read(victim).unwrap();
    let region = pack_blob_region(&bytes);
    assert!(region > 0, "the pack has no blob region to damage");
    bytes[0] ^= 0xff;
    fs::write(victim, &bytes).unwrap();

    let out = sb.prune(&[]);
    let text = stdout(&out);
    let code = out.status.code();

    assert_eq!(
        code,
        Some(3),
        "an unreadable pack leaves the survey incomplete: {text}{}",
        stderr(&out)
    );
    assert!(
        text.contains("unknown ("),
        "the damaged pack is reported unknown, with a reason: {text}"
    );
    assert!(
        text.contains("RESULT      : INCOMPLETE"),
        "the survey says it did not finish reading: {text}"
    );
    assert!(
        text.contains("NOT adopted"),
        "one pack that cannot be verified refuses the whole adoption: {text}"
    );

    // The reason is the verifier's own, and it names the pack by its prefix.
    let short = unix_rel(victim, &sb.repo);
    let short = short.rsplit('/').next().unwrap();
    assert!(
        text.contains(&short[..12]),
        "the unknown carries the pack's identity: {text}"
    );
}

/// `--apply` is refused, in as many words, and it is refused **before** the
/// repository is opened — proved by pointing it at a path that does not exist,
/// where an open would have failed with a different message.
#[test]
fn apply_is_refused_naming_what_a_safe_delete_would_need() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    strand_packs(&sb);

    let before = tree_bytes(&sb.repo);
    let out = sb.prune(&["--apply"]);
    let err = stderr(&out);
    let after = tree_bytes(&sb.repo);

    assert_eq!(
        out.status.code(),
        Some(3),
        "a refused apply is a `3`: {err}"
    );
    for capability in [
        "repository-wide lock",
        "conditional delete",
        "trustworthy backend modification time",
    ] {
        assert!(
            err.contains(capability),
            "the refusal must name `{capability}`: {err}"
        );
    }
    assert!(
        err.contains("nothing was read or written"),
        "the refusal must say it touched nothing: {err}"
    );
    assert!(stdout(&out).is_empty(), "a refusal writes no report");
    assert_eq!(
        before, after,
        "a refused apply must not touch the repository"
    );

    // The refusal happens before the destination is dialled: a path that is not
    // a repository is refused for the same reason, not for being absent.
    let missing = sb.dir.path().join("no-such-repository");
    let out = sb
        .command()
        .args([
            "prune-orphans",
            "--apply",
            "--repo",
            missing.to_str().unwrap(),
            "--key-file",
            sb.key.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "the refusal must not depend on the destination answering"
    );
    assert!(
        stderr(&out).contains("repository-wide lock"),
        "the same refusal for a destination that was never reached: {}",
        stderr(&out)
    );
}

/// Every mode leaves the repository byte-identical, including the one that
/// reports the most.
#[test]
fn no_mode_writes_to_the_repository() {
    for (label, extra) in [
        ("dry run", Vec::new()),
        ("--dry-run", vec!["--dry-run"]),
        ("json", vec!["--json"]),
    ] {
        let sb = Sandbox::new(2, 200_000);
        let push = sb.push();
        assert!(push.status.success(), "{}", stderr(&push));
        strand_packs(&sb);

        let before = tree_bytes(&sb.repo);
        let out = sb.prune(&extra);
        let after = tree_bytes(&sb.repo);

        assert!(out.status.success(), "{label}: {}", stderr(&out));
        assert_eq!(before, after, "{label} changed the repository");
        assert!(!before.is_empty(), "{label}: the fixture must have packs");
    }
}

/// `--json` is one object on stdout, and it carries the audit facts the human
/// report shortens: full pack ids, and a status per candidate.
#[test]
fn the_json_document_carries_full_ids_and_refuses_apply_structurally() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    let stranded = strand_packs(&sb);

    let out = sb.prune(&["--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).is_empty(),
        "`--json` writes nothing but the object: {}",
        stderr(&out)
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();

    assert_eq!(value["command"], "prune-orphans");
    assert_eq!(value["mode"], "dry_run");
    assert_eq!(value["apply"]["supported"], serde_json::json!(false));
    assert_eq!(
        value["apply"]["missing_capabilities"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "the three missing capabilities are named in the document too"
    );
    assert_eq!(value["next_push"]["kind"], "would_adopt");
    assert_eq!(
        value["packs"]["unindexed"],
        serde_json::json!(stranded.len())
    );
    assert_eq!(value["packs"]["unknown"], serde_json::json!(0));

    let candidates = value["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), stranded.len());
    for candidate in candidates {
        assert_eq!(candidate["status"], "verified");
        assert_eq!(
            candidate["id"].as_str().unwrap().len(),
            64,
            "the structured output carries the full id for audit"
        );
        assert_eq!(candidate["id_prefix"].as_str().unwrap().len(), 12);
    }

    // The repository's own id, not the path it was reached at.
    assert_eq!(value["repository_fingerprint"].as_str().unwrap().len(), 64);
}

/// A declared destination resolves like it does for every other reading
/// command: `--destination <name>` is the name in the config, and no path is
/// accepted in its place.
#[test]
fn a_destination_name_resolves_the_repository() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    strand_packs(&sb);

    let config = sb.dir.path().join("config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            // rustic_cache_dir (W289): prune-orphans opened through this
            // destination must keep its metadata cache inside the sandbox —
            // the config knob rather than only the env, so a Windows child
            // (whose cache root the Known Folder API owns) is relocated too.
            "rustic_cache_dir = '{}'\n\n[destinations.localbox]\nrepo = '{}'\nkey_file = '{}'\n",
            sb.dir.path().join("rustic-cache").display(),
            sb.repo.display(),
            sb.key.display()
        ),
    )
    .unwrap();

    let out = sb
        .command()
        .args(["prune-orphans", "--destination", "localbox"])
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(
        text.contains("would adopt all"),
        "`--destination` reaches the same repository `--repo` does: {text}"
    );
}

/// The report is written to be pasted into a ticket: it names the repository by
/// its own id and never by where it lives.
#[test]
fn the_report_carries_no_path_no_host_and_no_machine_name() {
    let sb = Sandbox::new(2, 200_000);
    let push = sb.push();
    assert!(push.status.success(), "{}", stderr(&push));
    strand_packs(&sb);

    for extra in [Vec::new(), vec!["--json"]] {
        let out = sb.prune(&extra);
        let text = stdout(&out) + &stderr(&out);
        let root = sb.dir.path().to_string_lossy().into_owned();
        assert!(
            !text.contains(&root),
            "the report must not carry the repository path: {text}"
        );
        assert!(
            !text.contains("w250-fixture"),
            "the report must not carry a machine or session identity: {text}"
        );
        assert!(
            !text.contains("key.json"),
            "the report must not name the key file: {text}"
        );
    }
}

/// A destination that cannot be read at all is a `3`, and says which kind of
/// nothing it found — a missing repository, not an unreadable one.
#[test]
fn an_unreachable_repository_is_a_three_with_a_reason() {
    let sb = Sandbox::new(1, 4096);
    // A key that reads, so the failure under test is the repository and not the
    // key: the two are different states and only one of them is this test's.
    store::persist_key_file(&sb.store_config(), &MasterKey::new()).unwrap();
    fs::create_dir_all(sb.dir.path().join("elsewhere")).unwrap();
    let missing = sb.dir.path().join("elsewhere/not-a-repository");

    let out = sb
        .command()
        .args([
            "prune-orphans",
            "--repo",
            missing.to_str().unwrap(),
            "--key-file",
            sb.key.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(3),
        "a repository that could not be read proves nothing: {}",
        stdout(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("no repository to read at this destination"),
        "the message separates absent from unreadable: {err}"
    );
    // And it still carries no path into the output.
    let root = sb.dir.path().to_string_lossy().into_owned();
    assert!(
        !err.contains(&root),
        "the error must not name the path: {err}"
    );
}
