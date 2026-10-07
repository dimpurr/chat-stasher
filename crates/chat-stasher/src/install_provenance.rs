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
//! check reads that file. The stage itself is read **only** when the index has
//! no line for the install being asked about — a stage sealed before this index
//! existed, an index whose line was lost, or an index that is damaged — and what
//! that read finds is written down, so the next question for the same install is
//! answered from memory. Nothing is ever concluded from a missing line: "this
//! index has not seen it" and "the stage has sealed nothing" stay different
//! states, and only the second can answer "no conflict" (invariant 1).
//!
//! The index only ever *caches* an answer the shards already hold, and that is
//! what makes it safe to keep: the shard is written first and durably, the index
//! line second, so an index line cannot describe a shard that is not there. A
//! lost line costs one re-read, never a shard.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

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
/// Used only to repair a damaged file: the whole set is already in hand (the
/// lines that parsed plus what the stage was just read for), and appending to a
/// file that will not parse would leave every later question walking the stage
/// again. The write is not atomic, deliberately: a torn rewrite costs the next
/// question a re-read of the stage, which is where every answer comes from
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

/// Every observation of `install_id` sealed anywhere in this stage's session
/// tree, read from the shards themselves.
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
fn walk(stage: &Path, install_id: &str) -> Result<Vec<Observation>> {
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
                    if record.get("install_id").and_then(|value| value.as_str()) != Some(install_id)
                    {
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

/// The provenance this stage has sealed under `install_id`.
///
/// Answers from the index when it holds a line for this install, and otherwise
/// reads the stage and writes down what it found. An empty answer therefore
/// always means "the stage was read and holds nothing", never "the index had
/// nothing to say".
///
/// The caller must hold the stage lock: this both reads the index and, on the
/// paths that fill it, appends to it.
///
/// 🔴 A read that finds nothing writes **nothing**. A line saying "this stage
/// has sealed nothing under this install" would be a claim, and it would be a
/// stale one the moment a shard carrying this install arrived from anywhere the
/// index does not watch — a restored or merged stage — which is the direction
/// that must not fail. The absence of a line costs one more read of the stage
/// and nothing else, so that is the price paid here.
///
/// A failure to record what the read found is reported on stderr and does **not**
/// fail the caller. The stage is the archive and the index is derived from it,
/// so the cost of a lost line is one re-read of the shards — the same class of
/// best-effort write the extension's own capture record is, and not worth
/// turning a sealed delivery into a refusal over.
pub fn sealed_observations_for(
    stage: &Path,
    machine: &str,
    install_id: &str,
) -> Result<Vec<Observation>> {
    match read_index(stage, machine)? {
        Index::Complete(observations) => {
            let known: Vec<Observation> = observations
                .into_iter()
                .filter(|observation| observation.install_id == install_id)
                .collect();
            if !known.is_empty() {
                return Ok(known);
            }
            let found = walk(stage, install_id)?;
            record(stage, machine, &found);
            Ok(found)
        }
        Index::Absent => {
            let found = walk(stage, install_id)?;
            record(stage, machine, &found);
            Ok(found)
        }
        Index::Damaged(kept) => {
            let found = walk(stage, install_id)?;
            let mut repaired = kept;
            for observation in &found {
                if !repaired.contains(observation) {
                    repaired.push(observation.clone());
                }
            }
            if let Err(error) = rewrite(stage, machine, &repaired) {
                eprintln!("native-host: install provenance index could not be repaired: {error:#}");
            }
            Ok(found)
        }
    }
}

/// Write down what a read of the stage found. Best-effort, by the contract on
/// [`sealed_observations_for`].
fn record(stage: &Path, machine: &str, found: &[Observation]) {
    for observation in found {
        if let Err(error) = append(stage, machine, observation) {
            eprintln!("native-host: install provenance index could not be written: {error:#}");
        }
    }
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

    /// The index answers, and the stage is **not** read to answer it: a shard
    /// holding a second, contradicting identity is both absent from the answer
    /// and absent from the file afterwards. This is the whole point of the
    /// change — the answer used to cost 196–205 s of reading every shard in a
    /// 12 GB stage.
    #[test]
    fn an_indexed_observation_answers_without_reading_the_stage() {
        let stage = tempfile::tempdir().expect("stage");
        let stage = stage.path();
        append(stage, MACHINE, &observation("Chrome")).expect("seed the index");
        let seeded = index_lines(stage);
        plant_shard(
            stage,
            "deepseek.abc",
            r#"{"install_id":"w9300000-0000-4000-8000-000000000000","browser":"Firefox"}"#,
        );

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
