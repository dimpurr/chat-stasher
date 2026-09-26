//! merge — one dashboard out of several destinations (29-UI-DESIGN §4.8, R10).
//!
//! `ui --destination a,b` reads each destination once, in the order named, and
//! merges the reports **at the row level**. This module is that merge and
//! nothing else: it takes reports, it returns rows and per-destination state,
//! and it never reads a repository, a clock or a config.
//!
//! The rules, each of which a test pins:
//!
//! * **The row key is `(machine, session_id)`.** Two destinations holding the
//!   same session id for the same machine are two *copies of one
//!   conversation*, not two conversations. That is the whole reason the merge
//!   exists, and it is why the badge is a badge and not a second row: the
//!   alternative — listing both — would inflate every count on the page by
//!   exactly the amount of redundancy the backup is supposed to provide.
//! * **The first destination named wins.** Where two copies disagree (their
//!   snapshots were taken at different times, so their last-message time and
//!   byte count can differ), the row's facts come from the first destination
//!   in `--destination` order that holds it. Naming the destinations is
//!   therefore also choosing the reading; the count line and the rows' own
//!   tooltips say which. The losers contribute their presence and nothing else.
//! * **Distinct and raw are both computed.** [`Merged::raw_sessions`] adds the
//!   per-destination totals; [`Merged::distinct_sessions`] counts merged rows.
//!   Neither is derived from the other, and both reach the page.
//! * **A destination that could not be read is a hole in the page, not a
//!   smaller page.** Its [`crate::ui::DestinationState::unreadable`] is
//!   non-empty (or it is absent from the rows entirely, when the read itself
//!   failed), which makes the whole dashboard report an INCOMPLETE read —
//!   naming that destination and only that one.
//!
//! What the merge deliberately does **not** do: pick a "best" copy on a
//! criterion the user cannot see, or drop a destination's rows to make the
//! counts agree.

use std::collections::{BTreeMap, BTreeSet};

use crate::fts;
use crate::search::{HostSnapshot, SearchReport};

use super::{DestinationState, IndexState, QueryResult, TextIndex, UiSession};

/// One destination's read, as `cmd_ui` obtained it.
///
/// `Err` is a destination that could not be read **at all** — the repository
/// would not open, the key was missing, the snapshot list would not list. It
/// still produces a [`DestinationState`] (with its reason) rather than being
/// skipped: a destination the run was asked for and did not read has to be
/// visible on the page as a hole, never as a destination that held nothing.
pub struct DestinationRead<'a> {
    pub label: String,
    pub outcome: Result<&'a SearchReport, String>,
}

/// The merged inventory, still whole: the launch filter is applied by
/// [`super::UiData::from_reports`], not here.
pub(super) struct Merged {
    pub sessions: Vec<UiSession>,
    pub destinations: Vec<DestinationState>,
    pub hosts: Vec<HostSnapshot>,
    pub machines_without_index: Vec<String>,
    pub machines_with_legacy_index: Vec<String>,
    /// The union of every destination's unreadable parts, in destination order.
    /// Empty only when every destination read in full.
    pub unreadable: Vec<String>,
    pub raw_sessions: usize,
    /// Merged rows over the whole inventory — the distinct count.
    pub distinct_sessions: usize,
    pub sessions_seen: usize,
    pub snapshots_scanned: usize,
    pub snapshots_in_repo: usize,
    pub data_blobs_read: usize,
    pub index_files_read: usize,
}

pub(super) fn merge(reads: &[DestinationRead<'_>]) -> Merged {
    let mut out = Merged {
        sessions: Vec::new(),
        destinations: Vec::with_capacity(reads.len()),
        hosts: Vec::new(),
        machines_without_index: Vec::new(),
        machines_with_legacy_index: Vec::new(),
        unreadable: Vec::new(),
        raw_sessions: 0,
        distinct_sessions: 0,
        sessions_seen: 0,
        snapshots_scanned: 0,
        snapshots_in_repo: 0,
        data_blobs_read: 0,
        index_files_read: 0,
    };
    // The key -> row position, so a later destination holding a session the
    // merge already has only adds a badge instead of a row.
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut without_index: BTreeSet<String> = BTreeSet::new();
    let mut legacy_index: BTreeSet<String> = BTreeSet::new();

    for (position, read) in reads.iter().enumerate() {
        let mut state = DestinationState {
            label: read.label.clone(),
            ..DestinationState::default()
        };
        match &read.outcome {
            Err(why) => {
                // The entire destination is the hole. Its counts stay at zero
                // because nothing was counted — not because it held nothing,
                // which is what the sentence on the page says in as many words.
                state.unreadable.push(why.clone());
                out.unreadable.push(why.clone());
            }
            Ok(report) => {
                state.snapshots_scanned = report.snapshots_scanned;
                state.snapshots_in_repo = report.snapshots_in_repo;
                state.sessions = report.hits.len();
                state.unreadable = report.unreadable.clone();
                state.machines_without_index = report.machines_without_index.clone();
                state.machines_with_legacy_index = report.machines_with_legacy_index.clone();

                out.snapshots_scanned += report.snapshots_scanned;
                out.snapshots_in_repo += report.snapshots_in_repo;
                out.sessions_seen += report.sessions_seen;
                out.data_blobs_read += report.data_blobs_read;
                out.index_files_read += report.index_files_read;
                out.raw_sessions += report.hits.len();
                out.unreadable.extend(report.unreadable.iter().cloned());
                without_index.extend(report.machines_without_index.iter().cloned());
                legacy_index.extend(report.machines_with_legacy_index.iter().cloned());

                for hit in &report.hits {
                    let key = (hit.machine.clone(), hit.session_id.clone());
                    match seen.get(&key) {
                        Some(&row) => {
                            // The copy already has the row; this destination
                            // only adds its name to it. Guarded against a
                            // repeat inside one destination (which `search`
                            // cannot produce, and which must not silently
                            // become a "×3" badge if it ever did).
                            let row: &mut UiSession = &mut out.sessions[row];
                            if row.destinations.last() != Some(&position) {
                                row.destinations.push(position);
                            }
                        }
                        None => {
                            // `index` is the row's position in the merged
                            // inventory — assigned here, where the position is
                            // known, so the two can never drift.
                            let index = out.sessions.len();
                            seen.insert(key, index);
                            let mut row = row_of(hit, position);
                            row.index = index;
                            out.sessions.push(row);
                        }
                    }
                }
                for host in &report.hosts {
                    merge_host(&mut out.hosts, host);
                }
            }
        }
        out.destinations.push(state);
    }

    out.distinct_sessions = out.sessions.len();
    out.hosts.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    out.machines_without_index = without_index.into_iter().collect();
    out.machines_with_legacy_index = legacy_index.into_iter().collect();
    out
}

/// One report row as a dashboard row, tagged with the destination that
/// supplied it — the same mapping [`super::UiData::from_report`] has always
/// done, plus the tag.
fn row_of(hit: &crate::search::SessionHit, destination: usize) -> UiSession {
    UiSession {
        index: 0,
        machine: hit.machine.clone(),
        harness: hit.harness.clone(),
        session_id: hit.session_id.clone(),
        short_id: hit.short_id(),
        shard_count: hit.shard_count,
        bytes: hit.bytes,
        first_unix: hit.first_unix,
        last_unix: hit.last_unix,
        time_why: hit.time_why.clone(),
        time_source: hit.time_source.clone(),
        title: hit.title.clone(),
        provenance: hit.provenance.clone(),
        line_count: hit.line_count,
        archive_time_unix: hit.archive_time_unix,
        data_blobs: hit.data_blobs,
        destinations: vec![destination],
    }
}

/// Keep the **newest** snapshot seen for a hostname.
///
/// Two destinations back up the same machine independently, so their newest
/// snapshots for it are different runs. The merged view is a union of what the
/// archive knows, and what it knows about a machine is its latest run — so the
/// newer of the two is the entry, fields and all (a machine table that took the
/// time from one snapshot and the index state from another would describe a
/// snapshot that never existed). A tie keeps the earlier-named destination,
/// which is the same "first named wins" rule the rows follow.
fn merge_host(hosts: &mut Vec<HostSnapshot>, host: &HostSnapshot) {
    match hosts.iter_mut().find(|h| h.hostname == host.hostname) {
        Some(existing) if existing.archive_time_unix >= host.archive_time_unix => {}
        Some(existing) => *existing = host.clone(),
        None => hosts.push(host.clone()),
    }
}

// --------------------------------------------------------------- the index

/// `/search` over more than one destination's local full-text index.
///
/// The index is built **per destination** (`index build --destination <name>`),
/// and its document keys are `<machine>/<session_id>` — the same key the merge
/// uses, which is what makes a union well defined: the copies of one session
/// are one document id, not two.
///
/// Three decisions, all of them the merge's own rules applied to text:
///
/// * **Coverage is the union.** A session is searchable here if *any*
///   destination's index holds it, and the page's coverage line then measures
///   the merged view against exactly that set. A destination with no index
///   therefore shows up as sessions the index cannot answer for — a
///   measurement — rather than as an absent destination.
/// * **A document matched twice is one hit.** The first destination to answer
///   supplies its rank and its excerpt, matching the row rule. Ranks from two
///   indexes are not comparable (each is a score over its own corpus), so the
///   merge never sorts across them: the hits keep destination order, each
///   destination's own rank order inside it. That is the honest reading —
///   "best within each copy" — where interleaving them by score would invent a
///   comparison between two corpora.
/// * **A part that cannot be read is reported, not skipped.** A destination
///   whose index is missing or unreadable does not stop the query — it is
///   visible through [`TextIndex::parts`] and through coverage — but an index
///   that *errors* while answering fails the whole query, because a partial
///   answer presented as an answer is the one thing a search page must not do.
pub struct MergedTextIndex {
    /// `(destination label, that destination's index root)`. `None` for an
    /// index root that does not exist at all, because the platform reported no
    /// cache directory to put one in.
    parts: Vec<(String, Option<fts::Index>)>,
}

impl MergedTextIndex {
    pub fn new(parts: Vec<(String, Option<fts::Index>)>) -> Self {
        Self { parts }
    }

    /// One destination's state, decided exactly as the single-destination
    /// dashboard has always decided it — same three states, same words, so a
    /// merged server and a single one describe an identical index identically.
    fn state_of(index: &Option<fts::Index>) -> IndexState {
        let index = match index {
            Some(index) => index,
            None => {
                return IndexState::Unreadable(
                    "this system reports no cache directory, so no local index can exist"
                        .to_string(),
                )
            }
        };
        if !index.db_path().exists() {
            return IndexState::Missing;
        }
        match index.summary() {
            Ok(summary) => IndexState::Ready(summary),
            Err(error) => IndexState::Unreadable(format!("{error:#}")),
        }
    }

    fn single(&self) -> bool {
        self.parts.len() <= 1
    }
}

impl TextIndex for MergedTextIndex {
    fn state(&self) -> IndexState {
        if self.single() {
            return match self.parts.first() {
                Some((_, index)) => Self::state_of(index),
                // A server with no destination at all cannot consult an index,
                // and says that rather than reporting a missing one: nothing
                // was looked for.
                None => IndexState::Unreadable(
                    "no destination was read, so no index was consulted".to_string(),
                ),
            };
        }
        let states: Vec<IndexState> = self
            .parts
            .iter()
            .map(|(_, index)| Self::state_of(index))
            .collect();
        if states
            .iter()
            .any(|state| matches!(state, IndexState::Ready(_)))
        {
            let mut ids: BTreeSet<String> = BTreeSet::new();
            let mut written: Option<i64> = None;
            let mut all_dated = true;
            for state in &states {
                if let IndexState::Ready(summary) = state {
                    ids.extend(summary.ids.iter().cloned());
                    match (written, summary.written_unix) {
                        (_, None) => all_dated = false,
                        (None, Some(t)) => written = Some(t),
                        (Some(prev), Some(t)) => written = Some(prev.min(t)),
                    }
                }
            }
            // The merged index is only as fresh as its oldest part, and if any
            // part records no write time at all, the union has none either: the
            // alternative is to stamp the newest part's time on text that came
            // from the oldest, which is exactly the claim `written_unix` is
            // there to prevent.
            return IndexState::Ready(fts::IndexSummary {
                ids,
                written_unix: if all_dated { written } else { None },
            });
        }
        if states
            .iter()
            .any(|state| matches!(state, IndexState::Unreadable(_)))
        {
            let reasons: Vec<String> = self
                .parts
                .iter()
                .zip(states.iter())
                .filter_map(|((label, _), state)| match state {
                    IndexState::Unreadable(reason) => Some(format!("{label}: {reason}")),
                    _ => None,
                })
                .collect();
            return IndexState::Unreadable(reasons.join("; "));
        }
        IndexState::Missing
    }

    fn query(&self, query: &str) -> Result<QueryResult, String> {
        if self.single() {
            return match self.parts.first() {
                Some((_, Some(index))) if index.db_path().exists() => match index.matches(query) {
                    Ok(Ok(set)) => Ok(QueryResult::Matches(set)),
                    Ok(Err(short)) => Ok(QueryResult::TooShort(short)),
                    Err(error) => Err(format!("{error:#}")),
                },
                // No usable index: the same answer `state()` gives, so the two
                // can never disagree about whether a query could be asked.
                _ => match self.state() {
                    IndexState::Unreadable(reason) => Err(reason),
                    _ => Err("no index has been built for this destination".to_string()),
                },
            };
        }
        let mut matches: Vec<fts::RankedMatch> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut truncated = false;
        let mut failed: Vec<String> = Vec::new();
        for (label, index) in &self.parts {
            let Some(index) = index else { continue };
            if !index.db_path().exists() {
                continue;
            }
            match index.matches(query) {
                Ok(Ok(set)) => {
                    truncated |= set.truncated;
                    for hit in set.matches {
                        if seen.insert(hit.id.clone()) {
                            matches.push(hit);
                        }
                    }
                }
                // A short query is a property of the query and the tokenizer,
                // not of one index, so every index would say the same thing.
                Ok(Err(_)) => {}
                Err(error) => failed.push(format!("{label}: {error:#}")),
            }
        }
        if !failed.is_empty() {
            return Err(format!(
                "could not query every destination's index ({})",
                failed.join("; ")
            ));
        }
        if matches.is_empty() {
            // Ask one usable index whether the query was answerable at all, so
            // "too short to look up" stays distinct from "looked, not there".
            for (_, index) in &self.parts {
                let Some(index) = index else { continue };
                if !index.db_path().exists() {
                    continue;
                }
                if let Ok(Err(short)) = index.matches(query) {
                    return Ok(QueryResult::TooShort(short));
                }
                break;
            }
        }
        Ok(QueryResult::Matches(fts::MatchSet { matches, truncated }))
    }

    fn placements(&self, query: &str, ids: &[String]) -> Result<Vec<fts::MatchPlace>, String> {
        if self.single() {
            return match self.parts.first() {
                Some((_, Some(index))) if index.db_path().exists() => index
                    .placements(query, ids)
                    .map_err(|error| format!("{error:#}")),
                _ => match self.state() {
                    IndexState::Unreadable(reason) => Err(reason),
                    _ => Err("no index has been built for this destination".to_string()),
                },
            };
        }
        // Each index is asked only about the documents it holds — no index
        // knows the other's ids — and the answers are reassembled in the order
        // asked, which is the caller's contract: one answer per document,
        // in the order given.
        let mut answered: BTreeMap<String, fts::MatchPlace> = BTreeMap::new();
        let mut failed: Vec<String> = Vec::new();
        for (label, index) in &self.parts {
            let Some(index) = index else { continue };
            if !index.db_path().exists() {
                continue;
            }
            let summary = match index.summary() {
                Ok(summary) => summary,
                Err(error) => {
                    failed.push(format!("{label}: {error:#}"));
                    continue;
                }
            };
            let mine: Vec<String> = ids
                .iter()
                .filter(|id| summary.ids.contains(*id))
                .cloned()
                .collect();
            if mine.is_empty() {
                continue;
            }
            match index.placements(query, &mine) {
                Ok(places) => {
                    if places.len() != mine.len() {
                        return Err(format!(
                            "`{label}`'s index returned {} placement(s) for the {} document(s) \
                             it was asked about",
                            places.len(),
                            mine.len()
                        ));
                    }
                    for (id, place) in mine.into_iter().zip(places) {
                        answered.entry(id).or_insert(place);
                    }
                }
                Err(error) => failed.push(format!("{label}: {error:#}")),
            }
        }
        if !failed.is_empty() {
            return Err(format!(
                "could not place matches in every destination's index ({})",
                failed.join("; ")
            ));
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            match answered.remove(id) {
                Some(place) => out.push(place),
                // The document came from a match, so some index held it; not
                // finding one is a broken read, and guessing it into "no
                // message number is claimed" would hide that behind a
                // plausible answer.
                None => {
                    return Err(format!(
                        "no destination's index holds a placement for `{id}`, though a match for \
                         it was returned"
                    ))
                }
            }
        }
        Ok(out)
    }

    fn parts(&self) -> Option<Vec<(String, IndexState)>> {
        if self.single() {
            return None;
        }
        Some(
            self.parts
                .iter()
                .map(|(label, index)| (label.clone(), Self::state_of(index)))
                .collect(),
        )
    }
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fixture;

    fn merged(data: &[(&str, &SearchReport)]) -> Merged {
        let reads: Vec<DestinationRead<'_>> = data
            .iter()
            .map(|(label, report)| DestinationRead {
                label: (*label).to_string(),
                outcome: Ok(*report),
            })
            .collect();
        merge(&reads)
    }

    /// The reason the merge exists: one conversation backed up twice is **one**
    /// row, and the page can say both how many conversations there are and how
    /// many copies.
    #[test]
    fn a_session_in_two_destinations_is_one_row_and_two_copies() {
        let first = fixture::report();
        let second = fixture::report_partner();
        let out = merged(&[("alpha", &first), ("beta", &second)]);

        assert_eq!(out.distinct_sessions, 5, "4 + 2 with one session shared");
        assert_eq!(out.raw_sessions, 6, "every copy counted");
        assert_eq!(out.sessions.len(), 5);

        let shared: Vec<&UiSession> = out
            .sessions
            .iter()
            .filter(|s| s.session_id.ends_with("eacbacc09765"))
            .collect();
        assert_eq!(shared.len(), 1, "the shared session is one row");
        assert_eq!(shared[0].destinations, vec![0, 1]);
        assert_eq!(
            out.sessions
                .iter()
                .filter(|s| s.destinations.len() == 1)
                .count(),
            4,
            "the sessions only one destination holds carry one destination"
        );
    }

    /// Where the copies disagree, the first one named supplies the row — so the
    /// order a user typed is the order that decides, and it is not an accident
    /// of which report happened to be read second.
    #[test]
    fn the_first_named_destination_supplies_the_row() {
        let first = fixture::report();
        let second = fixture::report_partner();
        let out = merged(&[("alpha", &first), ("beta", &second)]);
        let row = out
            .sessions
            .iter()
            .find(|s| s.session_id.ends_with("eacbacc09765"))
            .expect("the shared session is merged");
        assert_eq!(row.bytes, 100, "alpha's reading, not beta's 999");
        assert_eq!(row.shard_count, 2);

        // Reversing the order re-reads the same session from the other copy.
        let out = merged(&[("beta", &second), ("alpha", &first)]);
        let row = out
            .sessions
            .iter()
            .find(|s| s.session_id.ends_with("eacbacc09765"))
            .expect("the shared session is merged");
        assert_eq!(row.bytes, 999, "beta's reading when beta is named first");
        assert_eq!(row.destinations, vec![0, 1], "still one row, still both");
    }

    /// A destination with unreadable parts keeps its rows **and** floors the
    /// page. Both halves matter: dropping its rows would turn "we could not
    /// read this copy" into "this copy holds nothing".
    #[test]
    fn a_partly_unreadable_destination_keeps_its_rows_and_floors_the_page() {
        let first = fixture::report();
        let mut second = fixture::report_partner();
        second
            .unreadable
            .push("host `m-9`: snapshot dddddddd tree walk failed".into());
        let out = merged(&[("alpha", &first), ("beta", &second)]);

        assert_eq!(out.distinct_sessions, 5);
        assert!(
            out.destinations[0].complete(),
            "alpha read in full and must not be marked otherwise"
        );
        assert!(
            !out.destinations[1].complete(),
            "beta is the one that could not be read in full"
        );
        assert_eq!(out.destinations[1].unreadable.len(), 1);
        assert!(
            out.destinations[1]
                .unreadable
                .iter()
                .any(|u| u.contains("m-9")),
            "the part is attributed to the destination that reported it"
        );
        assert_eq!(out.unreadable.len(), 1, "and the union carries it too");
        assert_eq!(
            out.sessions
                .iter()
                .filter(|s| s.destinations.contains(&1))
                .count(),
            2,
            "beta's own rows survive: an unreadable part is not an empty copy"
        );
    }

    /// A destination that could not be read **at all** is a hole with a name,
    /// and the other destination is untouched by it.
    #[test]
    fn a_destination_that_could_not_be_read_is_a_named_hole() {
        let first = fixture::report();
        let reads = vec![
            DestinationRead {
                label: "alpha".into(),
                outcome: Ok(&first),
            },
            DestinationRead {
                label: "beta".into(),
                outcome: Err("the key would not open".into()),
            },
        ];
        let out = merge(&reads);

        assert_eq!(out.destinations.len(), 2);
        assert!(out.destinations[0].complete());
        assert!(!out.destinations[1].complete());
        assert_eq!(out.destinations[1].sessions, 0);
        assert_eq!(out.destinations[1].snapshots_scanned, 0);
        assert!(
            out.destinations[1].unreadable[0].contains("key would not open"),
            "the reason travels with the destination"
        );
        assert_eq!(
            out.distinct_sessions, 4,
            "alpha's rows are all still here — the hole is the page's floor, not alpha's loss"
        );
        assert_eq!(out.raw_sessions, 4);
    }

    /// A machine's entry is the newest snapshot anyone holds for it, whole:
    /// taking the time from one copy and the index state from another would
    /// describe a snapshot that never existed.
    #[test]
    fn a_machine_entry_is_the_newest_snapshot_of_it() {
        let first = fixture::report();
        let second = fixture::report_partner();
        let out = merged(&[("alpha", &first), ("beta", &second)]);
        let m1 = out
            .hosts
            .iter()
            .find(|h| h.hostname == "m-1")
            .expect("m-1 is in both");
        assert_eq!(m1.archive_time_unix, fixture::NOW - 60, "beta's is newer");
        assert!(
            m1.index_read_ok && m1.index_trusted,
            "and its fields come from that same snapshot"
        );
    }

    /// One report in, one report out — the merge is a no-op for a single
    /// destination, which is what keeps the ordinary dashboard exactly what it
    /// has always rendered.
    #[test]
    fn one_destination_merges_to_itself() {
        let only = fixture::report();
        let out = merged(&[("solo", &only)]);
        assert_eq!(out.distinct_sessions, only.hits.len());
        assert_eq!(out.raw_sessions, only.hits.len());
        assert_eq!(out.destinations.len(), 1);
        assert!(out.sessions.iter().all(|s| s.destinations == vec![0]));
        assert_eq!(out.machines_without_index, only.machines_without_index);
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::fts::{DocText, Index, SourceDoc};

    fn doc(title: &str, body: &str) -> DocText {
        DocText {
            title: title.into(),
            body: body.into(),
            message_offsets: Vec::new(),
        }
    }

    /// One destination's index, holding `docs` as `(id, body)`.
    fn index(root: &std::path::Path, docs: &[(&str, &str)]) -> Index {
        let index = Index::at(root.to_path_buf());
        let sources: Vec<SourceDoc> = docs
            .iter()
            .map(|(id, body)| SourceDoc {
                id: (*id).to_string(),
                source_sha256: format!("sha-{id}-{body}"),
            })
            .collect();
        let bodies: std::collections::BTreeMap<String, &str> = docs
            .iter()
            .map(|(id, body)| ((*id).to_string(), *body))
            .collect();
        index
            .build(&sources, |id| {
                Ok(doc(
                    "synthetic title",
                    bodies.get(id).copied().unwrap_or("synthetic"),
                ))
            })
            .unwrap();
        index
    }

    /// One destination's index whose documents are `messages`-long, with the
    /// offsets a real build records — which is what a placement is computed
    /// from.
    fn index_of(root: &std::path::Path, docs: &[(&str, &[&str])]) -> Index {
        let index = Index::at(root.to_path_buf());
        let sources: Vec<SourceDoc> = docs
            .iter()
            .map(|(id, _)| SourceDoc {
                id: (*id).to_string(),
                source_sha256: format!("sha-{id}"),
            })
            .collect();
        let bodies: std::collections::BTreeMap<String, &[&str]> = docs
            .iter()
            .map(|(id, msgs)| ((*id).to_string(), *msgs))
            .collect();
        index
            .build(&sources, |id| {
                let mut text = doc("synthetic title", "");
                for message in bodies.get(id).copied().unwrap_or(&[]) {
                    if !text.body.is_empty() {
                        text.body.push('\n');
                    }
                    text.message_offsets.push(text.body.chars().count());
                    text.body.push_str(message);
                }
                Ok(text)
            })
            .unwrap();
        index
    }

    fn state_words(index: &MergedTextIndex) -> Vec<(String, String)> {
        index
            .parts()
            .unwrap_or_default()
            .into_iter()
            .map(|(label, state)| {
                let word = match state {
                    IndexState::Ready(summary) => format!("ready:{}", summary.ids.len()),
                    IndexState::Missing => "missing".to_string(),
                    IndexState::Unreadable(_) => "unreadable".to_string(),
                };
                (label, word)
            })
            .collect()
    }

    /// A document held by both copies is **one** result: the same reason the
    /// rows merge, applied to text.
    #[test]
    fn a_document_in_two_indexes_is_one_hit() {
        let dir = tempfile::tempdir().unwrap();
        let a = index(
            &dir.path().join("a"),
            &[
                ("m-1/shared", "a hedgehog appears in both copies"),
                ("m-1/only-a", "only alpha has this"),
            ],
        );
        let b = index(
            &dir.path().join("b"),
            &[
                ("m-1/shared", "a hedgehog appears in both copies"),
                ("m-1/only-b", "only beta has this"),
            ],
        );
        let merged =
            MergedTextIndex::new(vec![("alpha".into(), Some(a)), ("beta".into(), Some(b))]);

        let IndexState::Ready(summary) = merged.state() else {
            panic!("two built indexes are ready");
        };
        assert_eq!(summary.ids.len(), 3, "the union of both document sets");

        let QueryResult::Matches(set) = merged.query("hedgehog").unwrap() else {
            panic!("a long-enough query is answered");
        };
        assert_eq!(set.matches.len(), 1, "one document, however many copies");
        assert_eq!(set.matches[0].id, "m-1/shared");

        let QueryResult::Matches(set) = merged.query("only alpha").unwrap() else {
            panic!("a long-enough query is answered");
        };
        assert_eq!(set.matches.len(), 1);
        assert_eq!(set.matches[0].id, "m-1/only-a");
    }

    /// One destination with no index is named as such rather than silently
    /// shrinking the search: the merged state is the union of what is
    /// readable, and `parts` is where the gap is attributable.
    #[test]
    fn a_destination_without_an_index_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let a = index(&dir.path().join("a"), &[("m-1/x", "synthetic body")]);
        let merged = MergedTextIndex::new(vec![
            ("alpha".into(), Some(a)),
            (
                "beta".into(),
                Some(Index::at(dir.path().join("never-built"))),
            ),
        ]);
        assert!(matches!(merged.state(), IndexState::Ready(_)));
        assert_eq!(
            state_words(&merged),
            vec![
                ("alpha".to_string(), "ready:1".to_string()),
                ("beta".to_string(), "missing".to_string()),
            ]
        );
    }

    /// Every destination missing is `Missing`, not `Ready` over an empty union:
    /// "no index anywhere" and "an index that holds nothing" are different
    /// answers, and the page words them differently.
    #[test]
    fn no_index_anywhere_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let merged = MergedTextIndex::new(vec![
            ("alpha".into(), Some(Index::at(dir.path().join("a")))),
            ("beta".into(), Some(Index::at(dir.path().join("b")))),
        ]);
        assert_eq!(merged.state(), IndexState::Missing);
    }

    /// The union is only as fresh as its oldest part, and a part with no write
    /// time takes the union's time away with it — the alternative is stamping
    /// the newest file's time on text that came from the oldest.
    #[test]
    fn the_union_is_as_fresh_as_its_oldest_part() {
        let dir = tempfile::tempdir().unwrap();
        let a = index(&dir.path().join("a"), &[("m-1/x", "synthetic body")]);
        let b = index(&dir.path().join("b"), &[("m-1/y", "synthetic body")]);
        let a_time = a.summary().unwrap().written_unix.expect("a real mtime");
        let b_time = b.summary().unwrap().written_unix.expect("a real mtime");
        let merged =
            MergedTextIndex::new(vec![("alpha".into(), Some(a)), ("beta".into(), Some(b))]);
        let IndexState::Ready(summary) = merged.state() else {
            panic!("both indexes are built");
        };
        assert_eq!(summary.written_unix, Some(a_time.min(b_time)));
    }

    /// A single destination is the ordinary server: `parts` says nothing extra,
    /// and state/query/placements answer exactly as the index does directly.
    #[test]
    fn one_destination_answers_exactly_as_its_index_does() {
        let dir = tempfile::tempdir().unwrap();
        let a = index(
            &dir.path().join("a"),
            &[("m-1/x", "a hedgehog appears here")],
        );
        let direct_state = a.summary().unwrap();
        let direct_hits = a.matches("hedgehog").unwrap().unwrap();
        let ids: Vec<String> = direct_hits.matches.iter().map(|m| m.id.clone()).collect();
        let direct_places = a.placements("hedgehog", &ids).unwrap();

        let merged = MergedTextIndex::new(vec![("alpha".into(), Some(a))]);
        assert!(
            merged.parts().is_none(),
            "one destination has no per-destination line to print"
        );
        let IndexState::Ready(summary) = merged.state() else {
            panic!("the index is ready");
        };
        assert_eq!(summary, direct_state);
        let QueryResult::Matches(set) = merged.query("hedgehog").unwrap() else {
            panic!("answered");
        };
        assert_eq!(set, direct_hits);
        assert_eq!(merged.placements("hedgehog", &ids).unwrap(), direct_places);
    }

    /// `placements` is asked per destination and reassembled in the order asked,
    /// so the caller's "one answer per document, in this order" contract holds
    /// even though no single index knows every document.
    ///
    /// The two documents are built so that **where** the query sits inside them
    /// differs — message 1 in one, message 0 in the other — and they are asked
    /// about in the reverse of the destinations' own order. A merge that
    /// answered by walking destinations instead of the requested order would
    /// swap the two answers, and a merge that asked only one index would return
    /// short.
    #[test]
    fn placements_are_reassembled_in_the_order_asked() {
        let dir = tempfile::tempdir().unwrap();
        let a = index_of(
            &dir.path().join("a"),
            &[("m-1/only-a", &["turn 0", "turn 1 hedgehog"])],
        );
        let b = index_of(
            &dir.path().join("b"),
            &[("m-1/only-b", &["hedgehog here", "turn 1"])],
        );
        let merged =
            MergedTextIndex::new(vec![("alpha".into(), Some(a)), ("beta".into(), Some(b))]);
        let ids = vec!["m-1/only-b".to_string(), "m-1/only-a".to_string()];
        let places = merged.placements("hedgehog", &ids).unwrap();
        assert_eq!(
            places,
            vec![
                crate::fts::MatchPlace::Message { ordinal: 0 },
                crate::fts::MatchPlace::Message { ordinal: 1 },
            ],
            "answers follow the ids asked about, not the destinations they live in"
        );
    }
}
