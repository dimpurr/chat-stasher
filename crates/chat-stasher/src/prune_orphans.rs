//! `prune-orphans`: what an operator may know about the packs no index file
//! names, and why this build will not delete any of them.
//!
//! [`crate::orphans`] answers "what are these packs, and can we reuse them": it
//! adopts them on open so a later push finds their blobs already present. This
//! module answers the operator's neighbouring question — how much is stranded,
//! how much of it the next push would adopt, and how much could not be read at
//! all — and answers it **without writing anything**. It opens the repository,
//! lists the backend once, reads the index files, and verifies each unindexed
//! pack's bytes with the same [`crate::packcheck::verify_pack`] the adopting
//! open uses. Every count it reports comes from that one listing, so a report's
//! totals and its unindexed set describe the same moment.
//!
//! # Why nothing here deletes
//!
//! "Unindexed" means *no index file names this pack*. It does not mean unused:
//! a retry can adopt a pack into an in-memory index and write a snapshot that
//! depends on it while the pack stays unindexed **on disk**, which is exactly
//! what [`crate::orphans`] does on purpose. So an unindexed pack may be the only
//! copy of a blob a snapshot already references, and deleting it would take
//! archived content with it.
//!
//! Closing that gap needs three capabilities this build does not have, and one
//! of them missing is enough to refuse:
//!
//! * **a repository-wide reader/writer lock** every client honours for its whole
//!   push or read. There is none. `.ingest.lock` is a local *stage* lock, and a
//!   local `flock` would not constrain another machine or another backend
//!   writer. Without it a sweep races a push on a machine this process cannot
//!   see.
//! * **a conditional delete** — delete exactly the object version that was
//!   checked, and fail if it changed. `rustic_core::ReadBackend` exposes
//!   `list_with_size`, which is a listing and a size; there is no version or
//!   ETag to condition on.
//! * **a trustworthy backend mtime** to age candidates by. The same interface
//!   carries no modification time at all, and the packing machine's own clock is
//!   not the backend's clock.
//!
//! The policy boundary is the fourth reason and the oldest: ADR-001 opens every
//! repository with `append_only:true` and ADR-016 made "we never prune" an
//! invariant. Disabling the flag to call rustic's broad `prune` is not this
//! module's decision to make — that command may rewrite indexes and repack
//! *indexed* data, where this would touch nothing but whole unindexed packs.
//!
//! So `--apply` is refused with those capabilities named, and the refusal is a
//! **`3`**: the requested apply could not be proven safe, which is the same
//! third state as "could not finish reading" and never a quiet `0`.
//!
//! # Reading, and what an unknown is
//!
//! A pack that does not verify is `unknown`, never "empty" or "nothing there":
//! the verifier's own refusal message is carried out with it, and its presence
//! makes the whole survey **incomplete** — an absence of candidates is then not
//! a proof that there were none. An index that names a pack the backend does not
//! list is a contradiction rather than an unknown: the reading finished, and the
//! two sources disagree.

use crate::orphans::{self, PackInventory};
use crate::store::StoreConfig;
use rustic_core::repofile::MasterKey;
use rustic_core::RepositoryBackends;

/// How one unindexed pack's bytes answered the verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The header decrypted, every blob it declares decrypted and decompressed
    /// to the length it gives, every plaintext hashed to the id the header
    /// names, and the pack's bytes hashed to the id it is stored under — the
    /// checks [`crate::packcheck::verify_pack`] makes, and the ones the adopting
    /// open must pass before it will reuse a pack.
    Verified,
    /// It could not be proven. `reason` is the verifier's own message, which
    /// names the pack by its id prefix.
    Unverified {
        /// Why this pack could not be read back as the bytes it is stored under.
        reason: String,
    },
}

/// One pack no index file names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The pack's full id, hex. The human report prints its first 12 characters
    /// and the structured one carries the whole thing, for audit.
    pub id: String,
    /// The size the backend listed for it.
    pub bytes: u64,
    /// What reading its bytes established.
    pub verdict: Verdict,
}

impl Candidate {
    /// The id prefix the human report prints.
    #[must_use]
    pub fn id_prefix(&self) -> &str {
        id_prefix(&self.id)
    }

    /// Whether the next push would reuse this pack's bytes.
    #[must_use]
    pub fn verified(&self) -> bool {
        matches!(self.verdict, Verdict::Verified)
    }
}

/// The 12-character id prefix every human-facing line in this command prints.
///
/// One function rather than one per call site: the whole reason a prefix is
/// printed at all is that the full id is an audit fact and the prefix is a
/// handle, and two rules for how much of the id is a handle would let the two
/// drift apart. The structured output carries the whole id.
#[must_use]
pub fn id_prefix(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// What the next open would do about the unindexed packs, by
/// [`crate::orphans::index_adopting`]'s rules.
///
/// Adoption is all-or-nothing: one pack that fails verification, or one index
/// entry the backend cannot satisfy, refuses the whole set — so a report that
/// listed only the packs that verified would overstate what a retry will reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextPush {
    /// The index names every pack the backend lists. There is nothing to adopt
    /// and nothing stranded, which is what a healthy repository looks like.
    NothingStranded,
    /// Every unindexed pack verified, so the next open adopts all of them and a
    /// later push uploads none of their blobs again.
    WouldAdopt,
    /// The next open adopts none of them. `why` names the fence that stopped it:
    /// an index entry the backend cannot satisfy, or a pack that did not verify.
    Refused {
        /// The refusal reason, in the adopting open's own words.
        why: String,
    },
}

/// Everything one read-only pass over the repository established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    /// The repository's own id from its config file. The same on every machine
    /// that reads this repository, and stable across a move or rename of the
    /// backend, which is why it is the fingerprint a report prints rather than
    /// the path or endpoint it was reached at.
    pub fingerprint: String,
    /// Which backend family the destination names — `local`, `opendal`, `rest`.
    /// Never the path, host or credentials behind it.
    pub backend: &'static str,
    /// Packs the backend listed.
    pub total_packs: usize,
    /// Bytes the backend listed for those packs.
    pub total_bytes: u64,
    /// The packs no index file names, id-sorted.
    pub candidates: Vec<Candidate>,
    /// Packs an index file names that the backend did not list. The plain index
    /// can still serve these out of the local metadata cache, so this is a
    /// disagreement to report, not a pack to count as stranded.
    pub missing: Vec<String>,
    /// What the next push would do about the candidates.
    pub next_push: NextPush,
}

impl Inventory {
    /// Packs no index file names.
    #[must_use]
    pub fn candidate_packs(&self) -> usize {
        self.candidates.len()
    }

    /// Backend bytes those packs occupy.
    #[must_use]
    pub fn candidate_bytes(&self) -> u64 {
        self.candidates.iter().map(|pack| pack.bytes).sum()
    }

    /// Candidates whose bytes read back as the ids they are stored under.
    #[must_use]
    pub fn verified_packs(&self) -> usize {
        self.candidates
            .iter()
            .filter(|pack| pack.verified())
            .count()
    }

    /// Backend bytes those verified packs occupy.
    #[must_use]
    pub fn verified_bytes(&self) -> u64 {
        self.candidates
            .iter()
            .filter(|pack| pack.verified())
            .map(|pack| pack.bytes)
            .sum()
    }

    /// Candidates that could not be read, so their status is unknown.
    #[must_use]
    pub fn unknown_packs(&self) -> usize {
        self.candidates
            .iter()
            .filter(|pack| !pack.verified())
            .count()
    }

    /// Backend bytes those unreadable packs occupy.
    #[must_use]
    pub fn unknown_bytes(&self) -> u64 {
        self.candidates
            .iter()
            .filter(|pack| !pack.verified())
            .map(|pack| pack.bytes)
            .sum()
    }
}

/// Which backend family a destination string names.
///
/// Only the scheme is reported. The rest of the string is a path on a local
/// destination and a host, user and root on a remote one, and neither belongs in
/// an operator report that may be pasted into a ticket.
fn backend_kind(repo_root: &str) -> &'static str {
    if repo_root.starts_with("opendal:") {
        "opendal"
    } else if repo_root.starts_with("rest:") {
        "rest"
    } else {
        "local"
    }
}

/// Survey one repository read-only: what the backend lists, what no index file
/// names, and whether each of those packs reads back as the bytes it is stored
/// under.
///
/// Writes nothing — not to the repository, not to an index file, not to a pack.
/// The same call is the whole of both modes: `--dry-run` prints it, and
/// `--apply` refuses before it is ever reached.
///
/// # Errors
///
/// If the repository cannot be opened, the backend cannot list its packs, or an
/// index file cannot be decrypted. A pack that cannot be read is **not** one of
/// these: it is reported as `unknown` on the inventory, which is what makes the
/// survey incomplete without making it a failure.
pub fn survey(
    cfg: &StoreConfig,
    backends: &RepositoryBackends,
    mk: &MasterKey,
) -> anyhow::Result<Inventory> {
    let repo = orphans::open_for_survey(cfg, backends, mk)?;
    let fingerprint = repo.config().id.to_hex().to_string();
    let PackInventory { listed, report } = orphans::survey_packs(&repo, backends)?;

    let verdicts = orphans::verify_candidates(backends, &report.unindexed, mk);
    let candidates: Vec<Candidate> = report
        .unindexed
        .iter()
        .zip(verdicts)
        .map(|((id, bytes), verdict)| Candidate {
            id: id.clone(),
            bytes: *bytes,
            verdict: match verdict {
                Ok(_) => Verdict::Verified,
                Err(reason) => Verdict::Unverified { reason },
            },
        })
        .collect();

    // The fences are checked in the order the adopting open checks them
    // (`index_adopting`): a missing entry refuses before verification is even
    // attempted, so a report says the same thing the next push would do.
    let next_push = if candidates.is_empty() {
        NextPush::NothingStranded
    } else if !report.missing.is_empty() {
        NextPush::Refused {
            why: format!(
                "{} pack(s) this index names are absent from the backend",
                report.missing.len()
            ),
        }
    } else if let Some(why) = candidates.iter().find_map(|pack| match &pack.verdict {
        Verdict::Unverified { reason } => Some(reason.clone()),
        Verdict::Verified => None,
    }) {
        NextPush::Refused { why }
    } else {
        NextPush::WouldAdopt
    };

    Ok(Inventory {
        fingerprint,
        backend: backend_kind(&cfg.repo_root),
        total_packs: listed.len(),
        total_bytes: listed.values().copied().sum(),
        candidates,
        missing: report.missing,
        next_push,
    })
}

/// Strip the path, host and credential out of a backend error.
///
/// The error text this command surfaces is built by `opendal`, `rustic` and the
/// OS, and none of it is ours to control: a local failure names the directory, an
/// SFTP one names the user and host. `docs-dev/privacy.md` and the issue rules in
/// `CONTRIBUTING.md` say a report may carry counts, sizes, hashes and prefixes,
/// and no path or account — so this reduces each token to the part that is about
/// the failure rather than about the machine:
///
/// * a token containing a path separator keeps only its last component, which is
///   enough to tell "permission denied on `config`" from "permission denied on
///   `keys`";
/// * a token containing `@` is a `user@host` credential and is replaced whole;
/// * a bare `host:port` or dotted-quad address is replaced whole;
/// * everything else — the words the error is actually made of — is left alone.
///
/// It cannot redact a bare hostname with no port and no `@`, because nothing in
/// the token distinguishes it from an ordinary word; the first two rules cover
/// the shapes `opendal` and the OS actually emit, and the limitation is named
/// here rather than left for a reader to discover.
#[must_use]
pub fn redact(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .split(' ')
        .map(redact_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One whitespace-delimited token of an error, reduced to its failure-relevant
/// part. See [`redact`] for the rules and what they cannot see.
fn redact_token(token: &str) -> String {
    if token.contains('@') {
        return "<host>".to_string();
    }
    if looks_like_endpoint(token) {
        return "<host>".to_string();
    }
    match token.rfind(['/', '\\']) {
        Some(cut) if cut + 1 < token.len() => format!("<path>/{}", &token[cut + 1..]),
        Some(_) => "<path>/".to_string(),
        None => token.to_string(),
    }
}

/// Whether a token is a `host:port` pair or a bare IPv4 address.
fn looks_like_endpoint(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '.');
    let (host, port) = match trimmed.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (trimmed, None),
    };
    if host.is_empty() || !host.contains('.') {
        return false;
    }
    // Exactly four numeric parts, so a version like `0.12.0` — which appears in
    // these errors as often as an address does — is not mistaken for one.
    let ipv4 = host.split('.').count() == 4
        && host
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    match port {
        // A trailing `:port` on a dotted name is an endpoint by shape alone.
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        None => ipv4,
    }
}

/// Whether `error` reads as a repository that is not there at all.
///
/// A missing repository and an unreadable one are different answers, and only
/// one of them is the operator's typo. Both are `3` — neither proves anything
/// about the archive — but the message has to say which.
#[must_use]
pub fn looks_absent(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("no such file")
        || text.contains("not found")
        || text.contains("does not exist")
        // rustic's own wording when a directory exists but holds no config: it
        // is not a repository, which is the same answer as "not there".
        || text.contains("no repository config file")
}

/// Turn a survey error into the line an operator reads.
#[must_use]
pub fn error_line(error: &anyhow::Error) -> String {
    let reason = redact(error);
    if looks_absent(error) {
        format!("no repository to read at this destination: {reason}")
    } else {
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::{backend_kind, error_line, redact, Candidate, Inventory, NextPush, Verdict};

    fn inventory(packs: &[(&str, u64, bool)]) -> Inventory {
        Inventory {
            fingerprint: "00".repeat(32),
            backend: "local",
            total_packs: packs.len(),
            total_bytes: packs.iter().map(|(_, bytes, _)| bytes).sum(),
            candidates: packs
                .iter()
                .map(|(id, bytes, ok)| Candidate {
                    id: (*id).to_string(),
                    bytes: *bytes,
                    verdict: if *ok {
                        Verdict::Verified
                    } else {
                        Verdict::Unverified {
                            reason: "pack 0123456789ab: damaged".to_string(),
                        }
                    },
                })
                .collect(),
            missing: Vec::new(),
            next_push: NextPush::WouldAdopt,
        }
    }

    #[test]
    fn a_pack_that_does_not_verify_is_counted_as_unknown_not_as_zero() {
        let inv = inventory(&[
            ("a".repeat(64).as_str(), 10, true),
            ("b".repeat(64).as_str(), 25, false),
        ]);
        assert_eq!(inv.candidate_packs(), 2);
        assert_eq!(inv.candidate_bytes(), 35);
        assert_eq!(inv.verified_packs(), 1);
        assert_eq!(inv.verified_bytes(), 10);
        // The unknown is a count and a size, never an absence.
        assert_eq!(inv.unknown_packs(), 1);
        assert_eq!(inv.unknown_bytes(), 25);
    }

    #[test]
    fn an_empty_survey_is_a_measurement_not_a_fallback() {
        let inv = inventory(&[]);
        assert_eq!(inv.candidate_packs(), 0);
        assert_eq!(inv.candidate_bytes(), 0);
        assert_eq!(inv.unknown_packs(), 0);
        assert_eq!(inv.total_packs, 0);
    }

    #[test]
    fn a_candidate_reports_the_id_prefix_the_human_report_prints() {
        let id = "0123456789abcdef".repeat(4);
        let inv = inventory(&[(&id, 1, true)]);
        assert_eq!(inv.candidates[0].id_prefix(), "0123456789ab");
    }

    #[test]
    fn a_short_or_empty_id_is_printed_whole_rather_than_panicking() {
        let inv = inventory(&[("abc", 1, true)]);
        assert_eq!(inv.candidates[0].id_prefix(), "abc");
    }

    #[test]
    fn only_the_backend_scheme_reaches_a_report_never_the_path_behind_it() {
        assert_eq!(backend_kind("/Users/someone/archive"), "local");
        assert_eq!(backend_kind("opendal:sftp"), "opendal");
        assert_eq!(backend_kind("rest:https://user@host/repo"), "rest");
        // A Windows drive letter looks like a scheme to a naive split and is not
        // one; the family check is on the known schemes, so it stays local.
        assert_eq!(backend_kind(r"C:\Users\someone\archive"), "local");
    }

    #[test]
    fn a_repository_path_in_an_error_is_redacted_to_its_last_component() {
        assert_eq!(
            redact(&anyhow::anyhow!(
                "open /Users/someone/archive/repo: permission denied"
            )),
            "open <path>/repo: permission denied"
        );
        assert_eq!(
            redact(&anyhow::anyhow!(
                r"read C:\Users\someone\repo\config failed"
            )),
            "read <path>/config failed"
        );
    }

    #[test]
    fn a_host_and_a_credential_are_redacted_while_a_version_number_is_not() {
        assert_eq!(
            redact(&anyhow::anyhow!("connect to sftp://user@host/repo failed")),
            "connect to <host> failed"
        );
        assert_eq!(
            redact(&anyhow::anyhow!("dial 10.0.0.7:22 failed")),
            "dial <host> failed"
        );
        // A three-part version is not an address, and an error that names one
        // must still read as itself.
        assert_eq!(
            redact(&anyhow::anyhow!("rustic_core 0.12.0 refused")),
            "rustic_core 0.12.0 refused"
        );
    }

    #[test]
    fn a_missing_repository_reads_as_absent_while_an_unreadable_one_does_not() {
        let absent = anyhow::anyhow!("open /tmp/repo: No such file or directory (os error 2)");
        assert!(super::looks_absent(&absent));
        assert!(error_line(&absent).starts_with("no repository to read at this destination:"));

        // The shape a directory with no config file in it takes: present, and
        // still not a repository.
        let no_config = anyhow::anyhow!("No repository config file found for /tmp/elsewhere/repo.");
        assert!(super::looks_absent(&no_config));
        assert!(error_line(&no_config).starts_with("no repository to read at this destination:"));

        let denied = anyhow::anyhow!("open /tmp/repo: permission denied");
        assert!(!super::looks_absent(&denied));
        assert_eq!(error_line(&denied), "open <path>/repo: permission denied");
    }
}
