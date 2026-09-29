//! B79 regression: `read --all-machines` and `verify --level l3` keep their
//! privacy-safe default labels, while `--full-ids` exposes the exact stage key
//! for a deliberate stage/archive comparison.
//!
//! Every repository, key, stage, and HOME/XDG directory below is synthetic and
//! isolated in a tempfile. The payload is a synthetic JSONL line.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MACHINE: &str = "b79-fixture-machine";
const SESSION: &str = "opencode.b79-fixture.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

/// ADR-034 added one line to `read --all-machines`: the body-cache state line
/// (`[read] body cache     : not used (bulk read, ADR-034)`). Bumping the pinned
/// digest is the point of this constant — the addition could not be made
/// quietly — and the delta was checked before it was bumped: removing exactly
/// that one line from the canonical report reproduces the previous digest
/// (`c378728e…`), so nothing else about the report moved.
///
/// W257 · bumped again, and the delta checked the same way. These two fixtures
/// run their commands with `--keep-ssh-masters`, and the ssh-master teardown
/// line (`[reap] skipped (--keep-ssh-masters)`) used to be written to **stdout**
/// — so it was part of what these digests pinned. It is housekeeping, not the
/// report, and it now goes to stderr (W253 C1: it made `search --json` /
/// `overview --json` unparseable). Neither digest is a re-derivation of a
/// changed claim: re-inserting exactly that one line at the position the old
/// output had it (last, for `read`; immediately before `[verify] RESULT` for
/// `verify`) reproduces the previous digest byte for byte, so nothing else in
/// either report moved. These constants now double as a regression test for
/// that: any future stdout pollution lands in the digest.
///
/// W257 · the read digest moved a third time, and the delta checked the same
/// way: `read --all-machines` now says `all-machines (every snapshot,
/// cumulatively)` where it said `all-machines (newest snapshot per hostname)`.
/// The implementation had been cumulative since ADR-021, so the old header was
/// the last place still claiming the wrong behaviour (W253 C5). Substituting
/// the old header text back reproduces the previous digest exactly
/// (`3e031951…`), so that one line is the whole delta. The verify digest did
/// not move, which is the same check from the other side.
const READ_DEFAULT_REPORT_SHA256: &str =
    "73adef7e6f72091247bdf19cdca8031b00e4004369074d2a316a1269fafd29b0";
const VERIFY_DEFAULT_REPORT_SHA256: &str =
    "f62693877a728e95bef872ef4bd419bea21f6f643b833d3edd1da130efdb29d7";

struct Fixture {
    sandbox: tempfile::TempDir,
    stage: PathBuf,
    repo: PathBuf,
    key: PathBuf,
}

fn config(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: Default::default(),
        cache_dir: None,
        no_cache: false,
    }
}

fn make_fixture() -> Result<Fixture, Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let stage = sandbox.path().join("stage");
    let repo = sandbox.path().join("repo");
    let key = sandbox.path().join("masterkey.json");
    let cfg = config(&repo, &key);
    let masterkey = MasterKey::new();

    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        MACHINE,
        SESSION,
        &[r#"{"synthetic":true,"seq":1}"#.to_string()],
    )?;
    store::persist_key_file(&cfg, &masterkey)?;
    BackupStore::new(cfg, MACHINE.to_string()).push(&stage, &masterkey)?;

    Ok(Fixture {
        sandbox,
        stage,
        repo,
        key,
    })
}

fn isolated_command(sandbox: &Path) -> Result<Command, Box<dyn Error>> {
    let home = sandbox.join("home");
    fs::create_dir_all(&home)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    Ok(command)
}

fn run_read(fixture: &Fixture, full_ids: bool) -> Result<Output, Box<dyn Error>> {
    let mut command = isolated_command(fixture.sandbox.path())?;
    command
        .args(["read", "--all-machines", "--repo"])
        .arg(&fixture.repo)
        .args(["--key-file"])
        .arg(&fixture.key)
        .args(["--keep-ssh-masters"]);
    if full_ids {
        command.arg("--full-ids");
    }
    Ok(command.output()?)
}

fn run_verify(fixture: &Fixture, full_ids: bool) -> Result<Output, Box<dyn Error>> {
    let mut command = isolated_command(fixture.sandbox.path())?;
    command
        .args(["verify", "--level", "l3", "--stage"])
        .arg(&fixture.stage)
        .args(["--repo"])
        .arg(&fixture.repo)
        .args(["--key-file"])
        .arg(&fixture.key)
        .args(["--machine", MACHINE, "--keep-ssh-masters"]);
    if full_ids {
        command.arg("--full-ids");
    }
    Ok(command.output()?)
}

#[test]
fn verify_unreadable_repository_exits_three() -> Result<(), Box<dyn Error>> {
    let fixture = make_fixture()?;
    let mut command = isolated_command(fixture.sandbox.path())?;
    let output = command
        .args(["verify", "--level", "l3", "--stage"])
        .arg(&fixture.stage)
        .args(["--repo"])
        .arg(fixture.sandbox.path().join("absent-repo"))
        .args(["--key-file"])
        .arg(&fixture.key)
        .args(["--machine", MACHINE, "--keep-ssh-masters"])
        .output()?;
    assert_eq!(output.status.code(), Some(3));
    Ok(())
}

#[test]
fn verify_completed_reconcile_mismatch_exits_one() -> Result<(), Box<dyn Error>> {
    let fixture = make_fixture()?;
    store::write_sealed_shard(
        StageWriter::Collect,
        &fixture.stage,
        MACHINE,
        "opencode.synthetic-later-session",
        &[r#"{"synthetic":true,"seq":2}"#.to_string()],
    )?;
    let output = run_verify(&fixture, false)?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("MISSING IN ARCHIVE"));
    Ok(())
}

fn text(output: Output) -> Result<String, Box<dyn Error>> {
    if !output.status.success() {
        return Err(format!(
            "fixture command failed with exit code {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn replace_field(line: &str, key: &str, replacement: &str) -> String {
    let Some(start) = line.find(key) else {
        return line.to_string();
    };
    let value_start = start + key.len();
    let value_end = line[value_start..]
        .find(char::is_whitespace)
        .map(|offset| value_start + offset)
        .unwrap_or(line.len());
    format!(
        "{}{}{}",
        &line[..value_start],
        replacement,
        &line[value_end..]
    )
}

fn canonical_report(text: &str, repo: &Path, stage: Option<&Path>) -> String {
    let repo = repo.to_string_lossy();
    let stage = stage.map(|path| path.to_string_lossy());
    let mut out = String::new();
    for line in text.lines() {
        let mut line = line.replace(repo.as_ref(), "<repo>");
        if let Some(stage) = stage.as_deref() {
            line = line.replace(stage, "<stage>");
        }
        if line.starts_with("  machine ") {
            line = replace_field(&line, "snapshot=", "<snapshot>");
            line = replace_field(&line, "time=", "<time>");
            line = replace_field(&line, "unix=", "<unix>");
        }
        if line.starts_with("[verify] L3 reconcile") {
            if let Some(start) = line.find("took ") {
                line = format!("{}took <duration>", &line[..start]);
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn default_reports_are_byte_stable_and_full_ids_match_stage_names() -> Result<(), Box<dyn Error>> {
    let fixture = make_fixture()?;
    let stage_session = fixture.stage.join("sessions").join(MACHINE).join(SESSION);
    assert_eq!(
        stage_session.file_name().and_then(|name| name.to_str()),
        Some(SESSION)
    );

    let default_read = text(run_read(&fixture, false)?)?;
    let default_verify = text(run_verify(&fixture, false)?)?;
    assert!(!default_read.contains(SESSION));
    assert!(!default_verify.contains(SESSION));
    let short = chat_stasher::id::short_session_id(SESSION);
    assert!(default_read.contains(&format!("session {short}")));
    assert!(default_verify.contains(&format!(" {short} ")));
    assert_eq!(
        sha256_hex(canonical_report(&default_read, &fixture.repo, None).as_bytes()),
        READ_DEFAULT_REPORT_SHA256
    );
    assert_eq!(
        sha256_hex(
            canonical_report(&default_verify, &fixture.repo, Some(&fixture.stage)).as_bytes()
        ),
        VERIFY_DEFAULT_REPORT_SHA256
    );

    let full_read = text(run_read(&fixture, true)?)?;
    let full_verify = text(run_verify(&fixture, true)?)?;
    assert!(full_read.contains(&format!("session {SESSION}")));
    assert!(full_verify.contains(&format!(" {SESSION} ")));
    Ok(())
}
