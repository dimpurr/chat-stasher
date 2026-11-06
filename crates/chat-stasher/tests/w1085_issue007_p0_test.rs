//! W1085 — ISSUE-007 P0 acceptance, at the command surface.
//!
//! The four P0 behaviours the issue's own acceptance names, each pinned by a
//! real binary run against a synthetic archive (machine ids, counts and cache
//! counters only — the privacy line the whole CLI keeps, asserted here too):
//!
//! * **S2 / F2 — the one-line refusal.** A declared destination whose `env:`
//!   credentials the current process cannot resolve answers `search` /
//!   `export` with one line naming the missing variables, the fix, and the
//!   usable alternative — before anything is dialed, exit `3`, no rustic
//!   chain on either stream.
//! * **F1 — the destination list.** `--help` (root and the destination-taking
//!   commands) and `status` list every declared destination with its
//!   two-state availability, from the same computation.
//! * **The quiet hourly log — no alarm per load.** An unresolved `env:`
//!   reference is no longer re-announced on every config load; the run that
//!   does not use the destination carries no line about it.
//! * **F3 / S3 — one scan, announced.** `export --dry-run` prints its header
//!   and an "enumerating" line before the scan, lines out one heartbeat per
//!   finished snapshot under the fixture override
//!   (`CHAT_STASHER_SCAN_PROGRESS_SECS=0`), and the counts never restart —
//!   the single sweep. The double-walk bug this replaces (a scan in
//!   `cmd_export`, a second one inside the export) would count 2×SNAPSHOTS
//!   events with the counter running to SNAPSHOTS twice.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

// --------------------------------------------------------------- fixtures

/// The two destinations the fixture declares: one whose credentials resolve
/// everywhere (`goodbox`, a real local repo built below), and one carrying
/// `env:` references the child process does not set (`coldstar`, the r2
/// shape from ISSUE-007 §2 F2 — declared, never configured).
const GOOD: &str = "goodbox";
const COLD: &str = "coldstar";
/// The variables `coldstar`'s options name — names only, never values.
const UNSET_KEY: &str = "W1085_UNSET_ACCESS_KEY_ID";
const UNSET_SECRET: &str = "W1085_UNSET_SECRET_ACCESS_KEY";

/// How many snapshots the fixture pushes — a full sweep must announce
/// exactly this many, once each.
const SNAPSHOTS: usize = 4;

fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..4)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

fn store_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo_path.to_string_lossy().into_owned(),
        key_file: key_path.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache (W289).
        cache_dir: None,
        no_cache: true,
    }
}

/// The sandbox: a config declaring both destinations, and a real `goodbox`
/// repo behind `SNAPSHOTS` pushes (one new session per push, so every
/// snapshot's tree has something to walk).
fn sandbox_with_archive() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("the-repo");
    let key = dir.path().join("the.key");
    let cfg = store_config(&repo, &key);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    for n in 1..=SNAPSHOTS {
        let stage = dir.path().join(format!("stage-{n}"));
        write_session(&stage, "machine-a", &format!("a{n}"), 2);
        BackupStore::new(cfg.clone(), "machine-a".to_string())
            .push(&stage, &mk)
            .unwrap();
    }
    let config_dir = dir.path().join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.{GOOD}]\nrepo = \"{}\"\nkey_file = \"{}\"\nno_cache = true\n\n\
             [destinations.{COLD}]\nrepo = \"opendal:s3\"\n\n\
             [destinations.{COLD}.options]\n\
             access_key_id = \"env:{UNSET_KEY}\"\n\
             secret_access_key = \"env:{UNSET_SECRET}\"\n",
            repo.display(),
            key.display(),
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("home")).unwrap();
    (dir, repo, key)
}

/// A `chat-stasher` binary run inside the sandbox envelope: nothing it reads
/// or writes is on the real machine, and the two fixture variables are
/// explicitly absent — a parent process cannot vouch for its own air.
fn bin(sandbox: &Path) -> Command {
    let home = sandbox.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE")
        .env_remove(UNSET_KEY)
        .env_remove(UNSET_SECRET);
    command
}

fn run(command: &mut Command) -> (i32, String, String) {
    let out = command.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// S5, pinned from the other side: none of the surfaces these tests cover may
/// carry conversation content — the marker below is the only place the
/// fixture's body text exists, and it must not leak into any stream.
fn assert_no_body_mark(stdout: &str, stderr: &str, what: &str) {
    for text in [stdout, stderr] {
        assert!(
            !text.contains("payload-a"),
            "{what} printed conversation bodies: {text}"
        );
    }
}

// ------------------------------------------------------- the one-line refusal

/// F2/S2: one line, three facts (the variables, the fix, the usable
/// alternative), nothing read, nothing dialed. The same destination used to
/// answer with a reqsign error chain naming nothing a user could act on.
#[test]
fn a_destination_without_credentials_refuses_in_one_line_of_three_facts() {
    let (dir, _repo, _key) = sandbox_with_archive();
    let sandbox = dir.path();
    let (code, stdout, stderr) = run(bin(sandbox).args([
        "export",
        "--destination",
        COLD,
        "--out",
        "wont-happen",
    ]));
    assert_eq!(code, 3, "refused, in the did-not-finish family");
    assert!(
        stdout.is_empty(),
        "stdout never reached its header — nothing was read: {stdout}"
    );
    assert!(stderr.contains(UNSET_KEY), "the first variable: {stderr}");
    assert!(stderr.contains(UNSET_SECRET), "the second variable: {stderr}");
    assert!(stderr.contains(GOOD), "the usable alternative: {stderr}");
    assert!(
        stderr.contains("`file:PATH`"),
        "the fix names the persistent forms: {stderr}"
    );
    assert!(
        stderr.contains("nothing was read"),
        "the refusal says nothing was read, not that nothing was found: {stderr}"
    );
    assert_eq!(
        stderr.trim_end().split('\n').count(),
        1,
        "one refusal line, not a paragraph: {stderr}"
    );
    assert!(
        !stderr.contains("rustic_core") && !stderr.contains("reqsign"),
        "no third-party error chain: {stderr}"
    );
    assert_no_body_mark(&stdout, &stderr, "the missing-credential refusal");

    // The same refusal answers `search` — the F1 agent walked both commands.
    let (code, stdout, stderr) = run(bin(sandbox).args(["search", "--destination", COLD]));
    assert_eq!(code, 3);
    assert!(stderr.contains(UNSET_KEY), "{stderr}");
    assert!(stderr.contains(GOOD), "{stderr}");
    assert_eq!(
        stderr.trim_end().split('\n').count(),
        1,
        "one refusal line: {stderr}"
    );
    assert_no_body_mark(&stdout, &stderr, "the search-side refusal");
}

// --------------------------------------------------------- the listed facts

/// F1: `status` and `--help` both list the declared destinations with the
/// availability an agent needs to pick one — names never again guessed out
/// of `[keys]` lines.
#[test]
fn status_and_help_list_destinations_with_two_state_availability() {
    let (dir, _repo, _key) = sandbox_with_archive();
    let sandbox = dir.path();
    let (code, status_stdout, status_stderr) = run(bin(sandbox).args(["status"]));
    assert!(
        code != 2,
        "status ran and reported; its health verdict is not this test's question"
    );
    let runs: Vec<(&str, Vec<&str>, String)> = vec![
        ("status", vec!["status"], {
            let mut all = String::new();
            all.push_str(&status_stdout);
            all.push_str(&status_stderr);
            all
        }),
        ("root --help", vec!["--help"], {
            let (code, stdout, stderr) = run(bin(sandbox).arg("--help"));
            assert_eq!(code, 0, "help exits 0");
            format!("{stdout}{stderr}")
        }),
        ("search --help", vec!["search", "--help"], {
            let (code, stdout, stderr) = run(bin(sandbox).args(["search", "--help"]));
            assert_eq!(code, 0, "help exits 0");
            format!("{stdout}{stderr}")
        }),
        ("export --help", vec!["export", "--help"], {
            let (code, stdout, stderr) = run(bin(sandbox).args(["export", "--help"]));
            assert_eq!(code, 0, "help exits 0");
            format!("{stdout}{stderr}")
        }),
    ];
    for (surface, _args, text) in &runs {
        assert!(text.contains(GOOD), "{surface} lists the good destination");
        assert!(text.contains(COLD), "{surface} lists the broken destination");
        assert!(
            text.contains("missing credentials"),
            "{surface} carries the failure state's words: {text}"
        );
        assert!(
            text.contains("available"),
            "{surface} names the state a usable destination gets: {text}"
        );
        assert!(
            text.contains(UNSET_KEY),
            "{surface} names the variable, where the fix starts: {text}"
        );
    }
    // `missing credentials` cannot appear without the destination whose
    // credentials they are, and the good destination's own line says why it
    // is usable in one place: `status` alone narrates both rows.
    let status_text = format!("{status_stdout}{status_stderr}");
    assert!(
        status_text.contains(&format!("[dest] {GOOD}")),
        "status prefixes each row with the destination's name: {status_text}"
    );
    assert_no_body_mark(&status_stdout, &status_stderr, "status");
}

// ------------------------------------------------------------ the quiet log

/// The quiet hourly run: an unresolved destination is not re-announced on
/// every config load. The load the fixture's `search` performs carries no
/// line about `coldstar`'s variables — the finding lives in the
/// availability listing, which this run's output never shows because it
/// speaks only about the destination that was named.
#[test]
fn a_run_that_avoids_the_broken_destination_says_nothing_about_it() {
    let (dir, _repo, _key) = sandbox_with_archive();
    let sandbox = dir.path();
    let (code, stdout, stderr) = run(bin(sandbox).args(["search", "--destination", GOOD]));
    assert_eq!(
        code, 0,
        "four sessions match, the whole destination was read: {stderr}"
    );
    let all = format!("{stdout}{stderr}");
    assert!(
        !all.contains("environment reference was omitted"),
        "no per-load alarm about the unresolved destination: {all}"
    );
    assert!(
        !all.contains(UNSET_KEY),
        "a run against `{GOOD}` does not carry `{COLD}`'s variables: {all}"
    );
    assert!(
        stdout.contains("machine-a"),
        "the scan itself answered, and this test caught a broken fixture otherwise: {stdout}"
    );
    assert!(
        stdout.contains(&format!("snapshots scanned: {SNAPSHOTS} of {SNAPSHOTS}")),
        "the report the heartbeat narrates is the same walk: {stdout}"
    );
    assert_no_body_mark(&stdout, &stderr, "the quiet run");
}

// -------------------------------------------------------- the announced scan

/// F3 directions 1 and 4 + S3: the dry run pays exactly one scan, the header
/// and the "enumerating" trace print **before** it, and with the fixture's
/// per-event override the heartbeat proves the sweep covered every snapshot
/// exactly once — a second full walk would print the same counts twice.
#[test]
fn a_dry_run_scans_the_destination_once_and_says_so_while_it_does() {
    let (dir, _repo, _key) = sandbox_with_archive();
    let sandbox = dir.path();
    let (code, stdout, stderr) = run(bin(sandbox)
        .env("CHAT_STASHER_SCAN_PROGRESS_SECS", "0")
        .args([
            "export",
            "--destination",
            GOOD,
            "--out",
            "dry-dir",
            "--dry-run",
        ]));
    assert_eq!(
        code, 0,
        "four sessions selected, the whole destination read, a dry run reports what the real one would: {stderr}"
    );

    // S3's first line: the header prints before the scan answers, and the
    // cost line only exists after it did.
    let header_at = stdout
        .lines()
        .position(|line| line.starts_with("[export] destination"))
        .expect("the header prints");
    let cost_at = stdout
        .lines()
        .position(|line| line.starts_with("[export] cost"))
        .expect("the dry run prices the plan");
    assert!(
        cost_at > header_at,
        "header before cost — F3's ten silent minutes came from this order turned around"
    );
    assert!(
        stdout.contains(&format!("sessions={}", SNAPSHOTS)),
        "the plan counts the four selected sessions: {stdout}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("enumerating the destination's snapshots")),
        "the scan announces itself on stderr before running: {stderr}"
    );

    // One sweep: every heartbeat line says `scanned k of n`, and together
    // the k's are exactly 1..=SNAPSHOTS once — never restarting, never
    // doubled. This is the assertion the double-walk build cannot pass: two
    // full walks over the same archive print twice the counters.
    let scan_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("] scan         : scanned "))
        .collect();
    let mut counts: Vec<usize> = scan_lines.iter().map(|line| scan_count(line)).collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        (1..=SNAPSHOTS).collect::<Vec<usize>>(),
        "one pass over {SNAPSHOTS} snapshots, each completed exactly once: {stderr}"
    );
    for line in &scan_lines {
        assert_eq!(
            scan_total(line),
            SNAPSHOTS,
            "the denominator is the repository's own listing: {line}"
        );
    }
    assert!(
        !stdout.contains("payload-a") && !stderr.contains("payload-a"),
        "S5: the announced scan prints counts and snapshots, never bodies"
    );
    // Nothing of `coldstar` leaks into a run against `goodbox` — the quiet
    // contract between destinations.
    assert!(
        !stderr.contains(UNSET_KEY) && !stdout.contains(UNSET_KEY),
        "the dry run speaks only about the named destination"
    );
}

/// `scanned 12 of 1159` — the walked count.
fn scan_count(line: &str) -> usize {
    head_number(line.split("scanned ").nth(1).unwrap_or_default())
}

/// `scanned 12 of 1159` — the repository's listing total.
fn scan_total(line: &str) -> usize {
    head_number(line.split(" of ").nth(1).unwrap_or_default())
}

fn head_number(text: &str) -> usize {
    text.split(' ').find_map(|word| word.parse().ok()).unwrap_or_default()
}
