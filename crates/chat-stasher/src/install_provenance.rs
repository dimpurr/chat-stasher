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
//! observation the stage holds, written by the one funnel every shard write
//! passes through (`shard_writer::write_shard` → [`note_shard_written`]).
//! The check reads that file. The stage itself is read **only** when the index
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
//! write this tool makes to the tree is a shard write through
//! `shard_writer::write_shard` — the host's `deliver`, `ingest`, `import`,
//! the collectors' sealing, restore included — and every such write, inside
//! that one funnel, is followed by the note that keeps the index current: a
//! line when the record carries an install identity the index does not hold,
//! and a renewal of the file's own modification time when it does not (an
//! already-held identity, or a collector record that cannot carry one at
//! all). The index's own mtime is therefore never older than the tree it was
//! written from — not only under the host's stage lock, but for the
//! collectors too, which write continuously and hold no stage lock (the
//! incident machine sealed 23 harness shards in the window's 35 minutes; an
//! index that ignored their writes was an index no delivery ever trusted).
//! An entry of the tree newer than the index therefore means the tree
//! changed without this tool — a restored or merged stage, the hazard
//! above — and the index is not trusted to answer for an install it has a
//! line for: the stage is read again. The check is a **stat walk** over the
//! tree (every entry is listed and stat'ed, no file is read), which is what
//! keeps the answer inside the budget while still failing closed. Three
//! limits are named, not hidden: a mutation that preserves the mtime of
//! every entry it touches is invisible to any mtime-based check, and so is
//! a mutation that lands while the walk is running, and a collector write
//! whose note failed (reported on stderr, best-effort by contract) leaves
//! the gate open until the next seal renews it. All three are written down
//! in `docs-dev/threat-model.md` next to D4.
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
/// Every install identity one shard's raw bytes carry, as sealed records say
/// them — the same extraction [`walk`] performs, shared so the two cannot
/// diverge.
///
/// Returns the identities in the order they appear, without deduplication:
/// sealing the same identity twice in one stage says one thing about it, and
/// the readers deduplicate by value.
fn observations_from(raw: &[u8]) -> Vec<Observation> {
    let mut out = Vec::new();
    for line in raw.split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(install_id) = record.get("install_id").and_then(|value| value.as_str()) else {
            continue;
        };
        if install_id.is_empty() {
            continue;
        }
        out.push(Observation {
            install_id: install_id.to_string(),
            browser: record
                .get("browser")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            profile_label: record
                .get("profile_label")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        });
    }
    out
}

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
                for observation in observations_from(&raw) {
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
/// the session tree is a shard write through the one funnel, and every such
/// write is followed by the note that keeps the index current — a line or a
/// renewal — so the index's own mtime is never older than the tree it was
/// written from. An entry newer than the index therefore means the tree
/// changed without this tool: a restored or merged stage, the hazard the
/// module header names. The index is then not trusted to answer for an
/// install it has a line for, and the stage is read again.
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

/// Record one observation, so the next question about this install is answered
/// without reading the stage, and — the half that keeps the gate honest — so
/// the index never falls behind the tree a shard write just changed.
///
/// The observation the index already holds is **not** appended a second time:
/// a delivery appends here on every seal, and a file that grew one line per
/// delivery would be a record of how many times something was delivered rather
/// than of what the stage holds — and every read of it would grow with that
/// count. The held case renews the index's modification time instead: the
/// content describes the current tree already (the write was a shard carrying
/// an identity the index has on the same terms), and the mtime is the one part
/// of the file that must move ahead of the shard, or the next delivery's gate
/// reads the tool's own write as an out-of-band change and re-reads the whole
/// stage (W930).
///
/// An index that is absent or damaged is appended to anyway: a repeated line
/// costs a reader nothing, and a missing one costs a re-read of the stage.
///
/// Best-effort in its callers, same contract as the rest of this module: a
/// failure is reported on stderr and does not fail the seal.
fn record_observation(stage: &Path, machine: &str, observation: &Observation) -> Result<()> {
    match read_index(stage, machine)? {
        Index::Complete(ref held) if held.contains(observation) => {
            renew_index_mtime_if_present(stage, machine)
        }
        Index::Complete(_) | Index::Absent | Index::Damaged(_) => {
            append(stage, machine, observation)
        }
    }
}

/// Renew the index's modification time, or do nothing when there is no index.
///
/// Renewal states one fact: everything the tool has written into the session
/// tree so far is described by the content that is already here. It is what a
/// shard write that carries no install identity does to the index (the
/// collector's shape — harness records — can never have changed an install
/// identity), and what a seal of an already-held identity does, and it is why
/// those writes leave the gate closed instead of opening it.
///
/// Absence is deliberately not a renewal: creating an empty index would assert
/// "this stage has sealed nothing under any install", and that is a claim only
/// a read of the stage can back (invariant 1). A stage with no index is
/// filled on its first question.
fn renew_index_mtime_if_present(stage: &Path, machine: &str) -> Result<()> {
    let path = index_path(stage, machine);
    let file = match fs::File::options().write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("renew install provenance {}", path.display()))
        }
    };
    file.set_modified(std::time::SystemTime::now())
        .with_context(|| format!("renew install provenance {}", path.display()))?;
    Ok(())
}

/// Note what one shard write just made true, so the index stays ahead of the
/// tree this tool writes — whatever producer wrote it.
///
/// This is the funnel: every sealed shard in a stage goes through
/// `shard_writer::write_shard` (the host's `deliver`, `ingest`, `import`, the
/// collectors, restore included), and that write is the one place every
/// producer shares, which is what makes the module's claim true in practice:
/// a tree entry newer than the index can only be a write that did not come
/// from this tool — the restored or hand-merged stage the gate exists for.
/// The note runs after the shard's own write is complete, so an index line
/// may never describe a shard that is not there.
///
/// A record that carries no install identity renews the index's mtime only
/// ([`renew_index_mtime_if_present`]); one that carries identities records
/// each identity, appending what is not held and renewing what is.
///
/// Best-effort, same contract: a failure is reported on stderr by the caller
/// and never fails the shard write — the stage is the archive and the index
/// is derived from it, so a lost note costs one re-read, never a shard.
pub fn note_shard_written(stage: &Path, machine: &str, raw: &[u8]) {
    let result = if memchr::memmem::find(raw, INSTALL_ID_FIELD).is_none() {
        renew_index_mtime_if_present(stage, machine)
    } else {
        (|| {
            for observation in observations_from(raw) {
                record_observation(stage, machine, &observation)?;
            }
            Ok(())
        })()
    };
    if let Err(error) = result {
        eprintln!("native-host: install provenance index could not be written: {error:#}");
    }
}

/// Record one observation at seal time, from its parts rather than from the
/// sealed bytes — the unit-level face of [`note_shard_written`], for the tests
/// that state a seal's bookkeeping directly.
///
/// Idempotent, and the held case renews the mtime — see [`record_observation`].
pub fn record_sealed(stage: &Path, machine: &str, install_id: &str, browser: &str, label: &str) {
    let observation = Observation {
        install_id: install_id.to_string(),
        browser: Some(browser.to_string()),
        profile_label: Some(label.to_string()),
    };
    if let Err(error) = record_observation(stage, machine, &observation) {
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

    /// Whether the gate the next question runs would trust the index — the
    /// property every seal has to leave behind it.
    fn the_index_answers_for_the_gate(stage: &Path, machine: &str) -> bool {
        !session_tree_changed_since(stage, &index_path(stage, machine)).expect("read the gate")
    }

    /// 🔴 W930 · A seal that adds no new line must still leave the index
    /// current: the *normal* shape is one install delivering many
    /// conversations, so from the second delivery on the observation is
    /// already held, and an index write that takes the early exit skips only
    /// the line — its file must still move ahead of the shard that delivery
    /// just wrote. Measured live against the branch's first cut (the
    /// `record_sealed` early return): the index never moved, every later
    /// delivery saw a tree newer than the index, and the 12 GB stage was
    /// re-read **on every single delivery** — the very cost the index exists
    /// to remove, paid minus one walk for the whole run (38–205 s a time).
    #[test]
    fn a_repeated_seal_keeps_the_index_current_for_the_gate() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        // The exact observation the second delivery will be asked to record —
        // held already, so the write it makes is the one that adds no line.
        let held = Observation {
            install_id: INSTALL.to_string(),
            browser: Some("Chrome".to_string()),
            profile_label: Some("Personal".to_string()),
        };
        append(stage, MACHINE, &held).expect("seed the index");
        // Old index: the first cut's append, before this delivery's shard.
        stamp(
            &index_path(stage, MACHINE),
            SystemTime::now() - std::time::Duration::from_secs(7200),
        );
        // The shard the second delivery just sealed: same install, same
        // browser — the observation the index already holds. Newer than the
        // index, stated on the clock rather than raced (see `stamp`).
        let shard = plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Chrome"}"#,
        );
        stamp(
            &shard,
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        assert!(
            !the_index_answers_for_the_gate(stage, MACHINE),
            "precondition: the shard the delivery sealed is newer than the index"
        );

        // The index write that delivery makes: the observation is held, so no
        // line is added — and the index must still come out *ahead* of the
        // shard, or the next delivery re-reads the whole stage.
        record_sealed(stage, MACHINE, INSTALL, "Chrome", "Personal");

        assert!(
            the_index_answers_for_the_gate(stage, MACHINE),
            "the index is still behind the shard the seal just wrote: the next delivery re-reads the stage"
        );
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

    /// 🔴 W930 · The funnel's no-provenance half: a shard write that carries no
    /// install identity — the collector's shape, harness records sealed into
    /// the same session tree continuously — renews the index's mtime and adds
    /// no line. An index left behind by such a write opened the gate for the
    /// next `deliver`, and the collectors that sealed 23 shards in the
    /// incident's 35-minute window would have kept it open for every tick.
    #[test]
    fn a_shard_with_no_provenance_renews_the_index_instead_of_losing_it() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        append(stage, MACHINE, &observation("Chrome")).expect("seed the index");
        stamp(
            &index_path(stage, MACHINE),
            SystemTime::now() - std::time::Duration::from_secs(7200),
        );
        // The harness shard the collector just sealed, newer than the index.
        let shard = plant_shard(
            stage,
            "claude-code.xyz",
            r#"{"captured_at":"2026-10-08T00:00:00Z","kind":"harness"}"#,
        );
        stamp(
            &shard,
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        assert!(
            !the_index_answers_for_the_gate(stage, MACHINE),
            "precondition: the collector's shard is newer than the index"
        );

        note_shard_written(
            stage,
            MACHINE,
            b"{\"captured_at\":\"2026-10-08T00:00:00Z\",\"kind\":\"harness\"}\n",
        );

        assert!(
            the_index_answers_for_the_gate(stage, MACHINE),
            "a provenance-free shard write left the index behind for the next delivery"
        );
        assert_eq!(
            index_lines(stage),
            format!(
                "{}\n",
                serde_json::to_string(&observation("Chrome")).expect("line")
            ),
            "no line was added: the write carried no identity"
        );
    }

    /// A write that carries no provenance must not *create* an index either: an
    /// empty index would answer every question with "this stage has sealed
    /// nothing under any install", and that is a claim only a read of the
    /// stage can back (invariant 1).
    #[test]
    fn a_note_with_no_provenance_creates_no_empty_index() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        note_shard_written(
            stage,
            MACHINE,
            b"{\"captured_at\":\"2026-10-08T00:00:00Z\",\"kind\":\"harness\"}\n",
        );
        assert!(
            !index_path(stage, MACHINE).exists(),
            "a collector write on a fresh stage must not assert an empty stage"
        );
    }

    /// The funnel records every identity one shard carries — a restored shard
    /// with several records is one write — and records nothing for an install
    /// id that appears inside a captured body, the same distinction
    /// [`walk`] makes.
    #[test]
    fn a_note_records_every_identity_a_shard_carries() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        note_shard_written(
            stage,
            MACHINE,
            format!(
                "{{\"install_id\":\"{INSTALL}\",\"browser\":\"Chrome\",\"profile_label\":\"Personal\",\"raw\":{{\"text\":\"the capture body quotes \\\"w9300000-0000-4000-8000-00000000beef\\\" inside it\"}}}}\n\
                 {{\"install_id\":\"another-930-install\",\"browser\":\"Firefox\"}}\n"
            )
            .as_bytes(),
        );
        let lines = index_lines(stage);
        assert!(
            lines.contains(INSTALL),
            "the named identity is recorded: {lines}"
        );
        assert!(
            lines.contains("another-930-install"),
            "both identities are recorded: {lines}"
        );
        assert!(
            !lines.contains("beef"),
            "a mention inside a body is not provenance: {lines}"
        );
    }
}
