//! What install provenance this stage has already sealed, written down where the
//! seal happens instead of re-read from every shard on every delivery (W930).
//!
//! D4's refusal (`inbox::install_identity_conflicts`) asks one question: "has
//! this stage already sealed this `install_id` under a different browser, or
//! under a different label the user actually named?". It used to answer by
//! reading **every shard in the stage**, parsing every line, on every
//! `deliver`.
//!
//! Measured 2026-10-07 on this machine's stage — 12,933 shards, 11.9 GB, 8,845
//! session directories, against an isolated copy and never the live stage — one
//! `deliver` took **196–205 s** (three runs: 196.0 s, 204.6 s, 195.6 s) and the
//! same delivery against an empty stage took **33–60 ms**. The extension's
//! request budget is 60 s (`lib/native-host.ts` `REQUEST_TIMEOUT_MS`), so every
//! delivery answered past the budget: the extension timed out, the host went on
//! to seal anyway, and no `ack` was ever read — the debt stayed pending, the leg
//! paused, and the next tick fetched and delivered the same conversation again.
//!
//! 96.5% of that stage is harness-collected shards (`claude-code`, `opencode`,
//! `codex`), which can never carry install provenance at all, and the remaining
//! time is dominated by parsing 11.9 GB of JSON: reading the same bytes without
//! parsing them costs ~16 s.
//!
//! So the evidence is written where it is produced:
//! `<stage>/meta/<machine>/install-provenance-v1.jsonl`, one line per sealed
//! observation, appended in the same stage lock the seal already holds. The
//! check reads that file. The stage itself is read **only** when the index
//! cannot answer — no index at all (a stage sealed before this index
//! existed), an index with no line for the install being asked about, an
//! index the session tree has outgrown, or an index that is damaged — and
//! what that read finds is written down, so the next question for the same
//! install is answered from memory. Nothing is ever concluded from a missing
//! line: "this index has not seen it" and "the stage has sealed nothing"
//! stay different states, and only the second can answer "no conflict"
//! (invariant 1).
//!
//! The index is trusted only while the session tree stands still. Every
//! write this tool makes to the tree is a shard write followed, inside the
//! same stage lock, by the index write that records it, so the index's own
//! mtime is never older than the tree it was written from. An entry of the
//! tree newer than the index therefore means the tree changed without this
//! tool — a restored or merged stage, the hazard above — and the index is
//! not trusted to answer for an install it has a line for: the stage is
//! read again. The check is a **stat walk** over the tree (every entry is
//! listed and stat'ed, no file is read), which is what keeps the answer
//! inside the budget while still failing closed. Two limits are named, not
//! hidden: a mutation that preserves the mtime of every entry it touches is
//! invisible to any mtime-based check, and so is a mutation that lands
//! while the walk is running. Both are written down in
//! `docs-dev/threat-model.md` next to D4.
//!
//! The index only ever *caches* an answer the shards already hold, and that is
//! what makes it safe to keep: the shard is written first and durably, the index
//! line second, so an index line cannot describe a shard that is not there. A
//! lost line costs one re-read, never a shard.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store;

/// The file, under `<stage>/meta/<machine>/`, that records which install
/// provenance this stage has sealed.
pub const INDEX_FILE: &str = "install-provenance-v1.jsonl";

/// The field name a sealed record spells its install identity with, as it
/// appears in the sealed bytes — the necessary condition the stage read is
/// bounded by.
///
/// The *name*, deliberately, and not the id: a filter for the id would decide
/// which files to parse from how a value happens to be spelled, while a record
/// that carries the field at all must spell the name this way. Every writer of
/// a sealed record here emits it verbatim (`inbox`'s `ShardRecord` serde form).
const INSTALL_ID_FIELD: &[u8] = b"\"install_id\"";

/// One sealed observation of an install identity: which `install_id`, from which
/// browser, under which label.
///
/// `browser` and `profile_label` are optional because the field set is the
/// extension's and it grows: a bundle that carried an `install_id` before these
/// two existed still says something true, and it is kept as what it says rather
/// than dropped or defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub install_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_label: Option<String>,
}

/// The index file for one machine's partition of one stage.
pub fn index_path(stage: &Path, machine: &str) -> PathBuf {
    stage.join("meta").join(machine).join(INDEX_FILE)
}

/// What one read of the index concluded. The three states are kept apart on
/// purpose: only [`Index::Complete`] may answer "the stage has sealed nothing
/// under this install", and a damaged file may not answer anything.
enum Index {
    /// No index file: this stage has never written one, so nothing is known.
    Absent,
    /// Every line parsed.
    Complete(Vec<Observation>),
    /// At least one line did not parse. The lines that did are carried, so a
    /// rewrite can keep them, but no question is answered from this file alone —
    /// the line that was lost may be the one being asked about.
    Damaged(Vec<Observation>),
}

impl Index {
    /// The lines that parsed, kept for a repair: a line the stage no longer
    /// backs is still kept, because the index is allowed to say more than the
    /// stage — an observation that says too much can only refuse a delivery,
    /// never admit one the stage would have refused — but never less.
    fn into_observations(self) -> Vec<Observation> {
        match self {
            Index::Absent => Vec::new(),
            Index::Complete(observations) | Index::Damaged(observations) => observations,
        }
    }
}

fn read_index(stage: &Path, machine: &str) -> Result<Index> {
    let path = index_path(stage, machine);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Index::Absent),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read install provenance {}", path.display()))
        }
    };
    let mut observations = Vec::new();
    let mut damaged = false;
    for line in raw.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Observation>(line) {
            Ok(observation) if !observation.install_id.is_empty() => observations.push(observation),
            // A short line, a torn write, a hand edit: the file no longer says
            // what it was written to say.
            _ => damaged = true,
        }
    }
    Ok(if damaged {
        Index::Damaged(observations)
    } else {
        Index::Complete(observations)
    })
}

/// Append one observation, creating the directory the first time.
///
/// The caller holds the stage lock (`inbox::seal_payload` takes it before the
/// duplicate scan), so one host at a time appends here — which is what makes the
/// read-then-answer loop in [`sealed_observations_for`] free of a race.
fn append(stage: &Path, machine: &str, observation: &Observation) -> Result<()> {
    let path = index_path(stage, machine);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("prepare install provenance {}", parent.display()))?;
    }
    let mut line = serde_json::to_vec(observation).context("serialise install provenance")?;
    line.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open install provenance {}", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("append install provenance {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush install provenance {}", path.display()))?;
    Ok(())
}

/// Replace the index with exactly `observations`, losing nothing and inventing
/// nothing.
///
/// Used by every path that reads the stage: the whole set is already in hand
/// (the lines that parsed plus what the stage was just read for), and appending
/// to a file that will not parse would leave every later question walking the
/// stage again. The write is not atomic, deliberately: a torn rewrite costs the
/// next question a re-read of the stage, which is where every answer comes from
/// anyway, and the shards are never touched.
fn rewrite(stage: &Path, machine: &str, observations: &[Observation]) -> Result<()> {
    let path = index_path(stage, machine);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("prepare install provenance {}", parent.display()))?;
    }
    let mut body = Vec::new();
    for observation in observations {
        serde_json::to_writer(&mut body, observation).context("serialise install provenance")?;
        body.push(b'\n');
    }
    fs::write(&path, &body)
        .with_context(|| format!("rewrite install provenance {}", path.display()))?;
    Ok(())
}

/// Every install identity sealed anywhere in this stage's session tree,
/// read from the shards themselves.
///
/// **Bounded by a necessary condition, never by a guess.** A record can only
/// carry an `install_id` if the file spells the field's name, so a shard that
/// never mentions it is skipped without being parsed — which is where the 196 s
/// went, since 96.5% of this stage is harness-collected shards that carry no
/// provenance field at all. The filter looks for the name and never for the id,
/// so a record that spells the same identity with a JSON escape is still found,
/// and every file it does match is parsed and compared as a record, so an id
/// that merely appears inside a captured body is not read as provenance. It can
/// therefore only save work, never change an answer. One limit, stated: a record
/// **hand-authored** to escape the field name itself would not be seen — this
/// tool never writes one that way, and the index answers for everything sealed
/// since it existed.
///
/// The walk extracts **every** install it finds, not just the one a question
/// was asked about: the scan already visits every shard that spells the field,
/// so keeping them all costs the parse and nothing more, and a read taken for
/// one install must not leave another install's answer stale behind a fresh
/// index mtime — the interleaving a hand-merged stage makes possible, and the
/// one direction that must not fail.
fn walk(stage: &Path) -> Result<Vec<Observation>> {
    let mut out: Vec<Observation> = Vec::new();
    let sessions = stage.join(store::SESSIONS_DIR);
    let machines = match fs::read_dir(&sessions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error).with_context(|| format!("read {}", sessions.display())),
    };
    for machine in machines {
        let machine = machine?;
        if !machine.file_type()?.is_dir() {
            continue;
        }
        for session in fs::read_dir(machine.path())? {
            let session = session?;
            if !session.file_type()?.is_dir() {
                continue;
            }
            for (_, shard) in store::sealed_shard_entries(&session.path())? {
                let raw = fs::read(&shard)
                    .with_context(|| format!("read install provenance from {}", shard.display()))?;
                if memchr::memmem::find(&raw, INSTALL_ID_FIELD).is_none() {
                    continue;
                }
                for line in raw.split(|byte| *byte == b'\n') {
                    let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
                        continue;
                    };
                    let Some(install_id) =
                        record.get("install_id").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    if install_id.is_empty() {
                        continue;
                    }
                    let observation = Observation {
                        install_id: install_id.to_string(),
                        browser: record
                            .get("browser")
                            .and_then(|value| value.as_str())
                            .map(str::to_string),
                        profile_label: record
                            .get("profile_label")
                            .and_then(|value| value.as_str())
                            .map(str::to_string),
                    };
                    if !out.contains(&observation) {
                        out.push(observation);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The newest modification time anywhere in this stage's session tree, or
/// `None` when the tree has no entry at all.
///
/// A **stat walk**: every entry is listed and stat'ed, and no file is read,
/// so the cost is a directory traversal — measured ~0.4 s over this machine's
/// stage (13,030 shards in 8,845 sessions), against the ~17 s reading the
/// same bytes costs and the 196–205 s parsing them did. It runs inside the
/// stage lock, so it is also the bound on what a concurrent delivery waits
/// for: well inside the 60-second `stage-unavailable` budget, which is
/// the extension's per-request budget and therefore the wait a delivery
/// may make on the lock (W930).
///
/// This is the index's invalidation signal. Every write this tool makes to
/// the session tree is a shard write, and every shard write is followed —
/// inside the same stage lock — by the index write that records it, so the
/// index's own mtime is never older than the tree it was written from. An
/// entry newer than the index therefore means the tree changed without this
/// tool: a restored or merged stage, the hazard the module header names. The
/// index is then not trusted to answer for an install it has a line for, and
/// the stage is read again.
///
/// One limit, stated: a mutation that preserves the mtime of every entry it
/// touches (a restore with `--preserve`, say) is invisible to this walk, as
/// is a mutation that lands while it runs. Both are named in
/// `docs-dev/threat-model.md` next to D4.
fn sessions_newest_mtime(stage: &Path) -> Result<Option<SystemTime>> {
    let sessions = stage.join(store::SESSIONS_DIR);
    let mut newest = match fs::metadata(&sessions) {
        Ok(metadata) => Some(metadata.modified()?),
        // A tree that is not there has no mtime to offer, and "nothing
        // changed" is the answer that keeps the fast path usable on a
        // stage whose sessions directory does not exist yet.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", sessions.display())),
    };
    let mut pending: Vec<PathBuf> = vec![sessions];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let modified = fs::metadata(&path)?.modified()?;
            newest = match newest {
                Some(newest) if newest >= modified => Some(newest),
                _ => Some(modified),
            };
            if entry.file_type()?.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(newest)
}

/// Whether the session tree holds an entry newer than the index file — the
/// signal that the tree changed without this tool, and the gate on the fast
/// path in [`sealed_observations_for`].
///
/// The caller holds the stage lock, so no write of this tool can land
/// between the index's read and this comparison; only an out-of-band
/// mutation can, and that is exactly what is being looked for.
fn session_tree_changed_since(stage: &Path, index: &Path) -> Result<bool> {
    let written = fs::metadata(index)?.modified()?;
    let newest = sessions_newest_mtime(stage)?;
    Ok(match newest {
        Some(newest) => newest > written,
        None => false,
    })
}

/// The provenance this stage has sealed under `install_id`.
///
/// Answers from the index when it holds a line for this install **and**
/// the session tree has not changed since the index was written, and
/// otherwise reads the stage and writes down what it found. An empty
/// answer therefore always means "the stage was read and holds nothing",
/// never "the index had nothing to say".
///
/// The caller must hold the stage lock: this both reads the index and,
/// on the paths that fill it, rewrites it.
///
/// 🔴 A read that finds nothing writes **nothing**. A line saying "this
/// stage has sealed nothing under this install" would be a claim, and it
/// would be a stale one the moment a shard carrying this install arrived
/// from anywhere the index does not watch — a restored or merged stage —
/// which is the direction that must not fail. The absence of a line
/// costs one more read of the stage and nothing else, so that is the
/// price paid here.
///
/// 🔴 The fast path's gate is [`session_tree_changed_since`]: an index
/// that holds a line for this install is still not trusted while the
/// tree is newer than the index, because a tree newer than the index is
/// a tree the index has not seen. And the read that follows extracts
/// **every** install in the stage, not just this one, so one read heals
/// the index for every install at once — a read taken for install A must
/// not leave install B's answer stale behind a fresh index mtime, which
/// is the interleaving a hand-merged stage makes possible.
///
/// A failure to record what the read found is reported on stderr and does
/// **not** fail the caller. The stage is the archive and the index is
/// derived from it, so the cost of a lost line is one re-read of the
/// shards — the same class of best-effort write the extension's own
/// capture record is, and not worth turning a sealed delivery into a
/// refusal over.
pub fn sealed_observations_for(
    stage: &Path,
    machine: &str,
    install_id: &str,
) -> Result<Vec<Observation>> {
    let index = read_index(stage, machine)?;
    if let Index::Complete(observations) = &index {
        let known: Vec<Observation> = observations
            .iter()
            .filter(|observation| observation.install_id == install_id)
            .cloned()
            .collect();
        if !known.is_empty() && !session_tree_changed_since(stage, &index_path(stage, machine))? {
            return Ok(known);
        }
    }
    // Every state that is not the fast path — no index, an index with
    // nothing to say about this install, an index the session tree has
    // outgrown, a damaged one — is answered by reading the stage, and
    // the read is written back whole: the lines that parsed plus what
    // the stage holds, so the next question, about any install, is
    // answered from memory.
    let found = walk(stage)?;
    let mut repaired = index.into_observations();
    for observation in &found {
        if !repaired.contains(observation) {
            repaired.push(observation.clone());
        }
    }
    if let Err(error) = rewrite(stage, machine, &repaired) {
        eprintln!("native-host: install provenance index could not be written: {error:#}");
    }
    Ok(found
        .into_iter()
        .filter(|observation| observation.install_id == install_id)
        .collect())
}

/// Record one observation at seal time, so the next question about this install
/// is answered without reading the stage. Best-effort, same contract.
///
/// Idempotent: an observation the index already holds is not written again. A
/// delivery appends here every time it seals, and a file that grew one line per
/// delivery would be a record of how many times something was delivered rather
/// than of what the stage holds — and every read of it would grow with that
/// count. An index that is absent or damaged is appended to anyway: a repeated
/// line costs a reader nothing, and a missing one costs a re-read of the stage.
///
/// The append is also what keeps the fast path's mtime gate honest for the
/// seal it belongs to: the shard is written first, this line second, so the
/// index's mtime is never older than the tree it was written from.
pub fn record_sealed(stage: &Path, machine: &str, install_id: &str, browser: &str, label: &str) {
    let observation = Observation {
        install_id: install_id.to_string(),
        browser: Some(browser.to_string()),
        profile_label: Some(label.to_string()),
    };
    if let Ok(Index::Complete(observations)) = read_index(stage, machine) {
        if observations.contains(&observation) {
            return;
        }
    }
    if let Err(error) = append(stage, machine, &observation) {
        eprintln!("native-host: install provenance index could not be written: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE: &str = "w930-machine";
    const INSTALL: &str = "w9300000-0000-4000-8000-000000000000";

    /// One sealed shard holding `browser`'s provenance for `install_id`, written
    /// the way `seal_payload` writes one.
    fn plant_shard(stage: &Path, session: &str, line: &str) -> PathBuf {
        let dir = store::session_shard_dir(stage, MACHINE, session).join("000");
        fs::create_dir_all(&dir).expect("shard bucket");
        let path = dir.join("000001.jsonl");
        fs::write(&path, format!("{line}\n")).expect("shard");
        path
    }

    fn observation(browser: &str) -> Observation {
        Observation {
            install_id: INSTALL.to_string(),
            browser: Some(browser.to_string()),
            profile_label: None,
        }
    }

    fn index_lines(stage: &Path) -> String {
        fs::read_to_string(index_path(stage, MACHINE)).unwrap_or_default()
    }

    /// Stamp `path`'s modification time, so a test says which of the two
    /// things the gate compares is the newer one.
    ///
    /// The gate decides on `newest mtime in the session tree > index mtime`,
    /// and a filesystem clock is coarse: Linux updates file times once per
    /// tick, so the two writes a test makes microseconds apart can compare
    /// **equal**, and "newer" is then false. Leaving that to how far apart two
    /// writes happened to land made the tree-newer test below pass on macOS
    /// and Windows and fail on Linux CI (2026-10-08, run 37721062117): the
    /// shard the test had just planted read as no newer than the index, the
    /// index answered, and the test reported that the stage had not been read.
    /// That is the limit the module header already names — a mutation that
    /// preserves the mtime of every entry it touches is invisible to the walk,
    /// and at tick granularity a mutation landing in the index's own tick is
    /// that case — so the test states the time it means instead of racing it.
    fn stamp(path: &Path, when: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open to stamp")
            .set_modified(when)
            .expect("set the modification time");
    }

    /// The index answers, and the stage is **not** read to answer it: a shard
    /// holding a second, contradicting identity is both absent from the answer
    /// and absent from the file afterwards. This is the whole point of the
    /// change — the answer used to cost 196–205 s of reading every shard in a
    /// 12 GB stage.
    ///
    /// The shard is planted **before** the index line, so the index's mtime is
    /// the newer of the two: the gate on this fast path compares the session
    /// tree against the index, and a tree newer than the index is a tree the
    /// index has not seen — the case below.
    #[test]
    fn an_indexed_observation_answers_without_reading_the_stage() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );
        append(stage, MACHINE, &observation("Chrome")).expect("seed the index");
        let seeded = index_lines(stage);

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(
            found,
            vec![observation("Chrome")],
            "the index is the answer"
        );
        assert_eq!(
            index_lines(stage),
            seeded,
            "nothing was appended: the stage was not read"
        );
    }

    /// 🔴 The fast path's gate: a session tree newer than the index is a tree
    /// the index has not seen, so the index is **not** the answer — the stage
    /// is read, and what it holds is what decides. This is the direction that
    /// must not fail: a shard that entered the tree without passing through
    /// `seal_payload` (a restored or merged stage) has to be visible to the
    /// conflict check, exactly as it was when every delivery read the whole
    /// stage.
    #[test]
    fn a_session_tree_newer_than_the_index_rereads_the_stage() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        append(stage, MACHINE, &observation("Chrome")).expect("seed the index");
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );

        // The tree is newer than the index — the mutation the gate exists to
        // catch — stated outright rather than left to the clock. See `stamp`.
        stamp(
            &index_path(stage, MACHINE),
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(
            found,
            vec![observation("Firefox")],
            "the stage is the answer: the shard the index had not seen is read"
        );
        let healed = index_lines(stage);
        assert!(
            healed.contains("Firefox"),
            "what the read found is written down: {healed}"
        );
    }

    /// A read taken for one install heals the index for **every** install: the
    /// walk extracts all of them, so a change that lands between two questions
    /// is caught by the question asked about *any* install — the index's mtime
    /// may not advance past a change the index has not seen. Without this, a
    /// merge that added an observation for install B would stay invisible to
    /// the next question about install A, once A's own question had refreshed
    /// the index's mtime.
    #[test]
    fn a_read_for_one_install_heals_the_index_for_every_install() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        plant_shard(
            stage,
            "deepseek.abc",
            &format!(r#"{{"install_id":"{INSTALL}","browser":"Chrome"}}"#),
        );
        append(stage, MACHINE, &observation("Chrome")).expect("seed the index");
        // A second install enters the tree after the index was written: the
        // shape a hand-merged stage has.
        plant_shard(
            stage,
            "deepseek.def",
            r#"{"install_id":"w9300000-0000-4000-8000-00000000beef","browser":"Firefox"}"#,
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(
            found,
            vec![observation("Chrome")],
            "the answer for the install asked about is what the stage holds"
        );
        let healed = index_lines(stage);
        assert!(
            healed.contains("beef"),
            "the read heals the index for the install it was not asked about: {healed}"
        );
    }

    /// 🔴 Invariant 1: an index with nothing to say about this install is not an
    /// answer. The stage is read, and what it holds is what decides.
    #[test]
    fn a_missing_index_is_not_an_answer() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Chrome","profile_label":"Personal"}"#,
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(
            found,
            vec![Observation {
                install_id: INSTALL.to_string(),
                browser: Some("Chrome".to_string()),
                profile_label: Some("Personal".to_string()),
            }]
        );
        assert!(
            index_lines(stage).contains("Personal"),
            "what the read found is written down, so the next question is answered from memory"
        );
    }

    /// An index that saw another install says nothing about this one: the stage
    /// is still read. This is the case a growing index has to keep honest.
    #[test]
    fn an_index_about_another_install_is_not_an_answer() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        append(
            stage,
            MACHINE,
            &Observation {
                install_id: "w9300000-0000-4000-8000-00000000beef".to_string(),
                browser: Some("Chrome".to_string()),
                profile_label: None,
            },
        )
        .expect("seed the index");
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(found, vec![observation("Firefox")]);
    }

    /// A torn line is not an answer either, and the file is repaired from the
    /// shards rather than left to send every later question back to the stage.
    #[test]
    fn a_damaged_index_is_repaired_from_the_stage() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        fs::create_dir_all(index_path(stage, MACHINE).parent().expect("index dir"))
            .expect("index directory");
        fs::write(
            index_path(stage, MACHINE),
            format!(
                "{{\"install_id\":\"{}\",\"browser\":\"Chrome\"}}\n{{\"install_id\":\"torn",
                "w9300000-0000-4000-8000-00000000beef"
            ),
        )
        .expect("damaged index");
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(found, vec![observation("Firefox")]);
        let repaired = index_lines(stage);
        assert!(repaired.contains("Firefox"), "the answer is written down");
        assert!(
            repaired.contains("beef"),
            "the line that parsed is kept, not dropped by the repair"
        );
        assert!(!repaired.contains("torn"), "the torn line is gone");
    }

    /// The byte filter is a *necessary* condition, never the comparison: an
    /// install id that appears inside a captured body is not provenance.
    #[test]
    fn an_install_id_inside_a_body_is_not_provenance() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        plant_shard(
            stage,
            "deepseek.abc",
            &format!(
                r#"{{"install_id":"w9300000-0000-4000-8000-00000000beef","raw":{{"text":"the capture body quotes \"{INSTALL}\" inside it"}}}}"#
            ),
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert!(
            found.is_empty(),
            "the body's mention is not a sealed identity"
        );
    }

    /// The filter may not lose a shard that *does* carry the identity, whatever
    /// else the file holds.
    #[test]
    fn the_filter_finds_provenance_past_a_large_body() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        let filler = "x".repeat(256 * 1024);
        plant_shard(
            stage,
            "deepseek.abc",
            &format!(
                r#"{{"raw":{{"text":"{filler}"}},"install_id":"{INSTALL}","browser":"Firefox","profile_label":"Work"}}"#
            ),
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(
            found,
            vec![Observation {
                install_id: INSTALL.to_string(),
                browser: Some("Firefox".to_string()),
                profile_label: Some("Work".to_string()),
            }]
        );
    }

    /// The filter is bounded by the field's **name**, not by the id's spelling:
    /// a record that writes the same identity with a JSON escape is still found.
    /// (`\u0077` is `w`, so this record's `install_id` is `INSTALL`.)
    #[test]
    fn an_escaped_identity_value_is_still_found() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"\u00779300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(found, vec![observation("Firefox")]);
    }

    /// The observations of one install in one answer are deduplicated: a stage
    /// that sealed the same identity twice says one thing about it.
    #[test]
    fn repeated_seals_of_one_identity_are_one_observation() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        let line = format!(r#"{{"install_id":"{INSTALL}","browser":"Chrome"}}"#);
        plant_shard(stage, "deepseek.abc", &line);
        plant_shard(stage, "plants", &line);

        let found = sealed_observations_for(stage, MACHINE, INSTALL).expect("answer");

        assert_eq!(found, vec![observation("Chrome")]);
    }

    /// A stage with no session tree at all is an empty stage, not an error: the
    /// shape `install_identity_conflicts` has always had for a stage that has
    /// never sealed anything.
    #[test]
    fn a_stage_with_no_sessions_is_empty_and_not_an_error() {
        let stage = tempfile::tempdir().expect("stage");
        let found = sealed_observations_for(stage.path(), MACHINE, INSTALL).expect("answer");
        assert!(found.is_empty());
    }

    /// A delivery records here on every seal, and the index says what the stage
    /// holds — not how many times something was delivered.
    #[test]
    fn sealing_the_same_identity_twice_writes_one_line() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        record_sealed(stage, MACHINE, INSTALL, "Chrome", "Personal");
        record_sealed(stage, MACHINE, INSTALL, "Chrome", "Personal");
        assert_eq!(
            index_lines(stage).lines().count(),
            1,
            "the second seal of one identity adds nothing"
        );
        record_sealed(stage, MACHINE, INSTALL, "Chrome", "Work");
        assert_eq!(
            index_lines(stage).lines().count(),
            2,
            "a different label is a different observation"
        );
    }
}
