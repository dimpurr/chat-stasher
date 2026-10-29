//! Debris an interrupted push leaves behind, and the one classification of it
//! every repository-scanning reader shares (ISSUE-006 step A).
//!
//! A push can be killed at any moment — a lid closed, a network changed, a
//! process was told to stop — and after it the repository can hold two kinds
//! of leftovers, which look alike until the bytes are read:
//!
//! * **torn** — the pack's *file* was left half-written under its final name
//!   (the write path of ADR-063 is a later, separate change). No index file
//!   names it, and it cannot be read as a pack: its length field declares a
//!   header the file cannot hold, or the header will not decrypt. It holds no
//!   content any snapshot can reach, so it belongs to no backup. Skipping it is
//!   safe in every direction: nothing is deleted, nothing is copied, and the
//!   next reader re-classifies it from the bytes.
//! * **stranded** — a *complete* pack whose index file was never written. No
//!   index file names it and it verifies complete, so the adopting open
//!   ([`crate::orphans`]) still reaches it and a later push uploads none of
//!   its blobs again. Today's behaviour, unchanged here.
//! * **corrupt** — an index file *does* name the pack and its bytes do not
//!   match what they claim to be. That is a checked-and-failed fact about a
//!   real backup: exit 1, exactly as before this module existed.
//!
//! Anything the classification cannot establish — the index files could not be
//! read, or a pack that is neither torn nor complete — is **unknown**, and the
//! rule for an unknown is fail closed: do not guess, do not skip, and do not
//! call it torn. A pack skipped on a guess is exactly the silent hole
//! [`CLAUDE.md`](crate) invariant 1 exists to prevent.
//!
//! The sentence every surface prints for torn debris is one constant
//! ([`INTERRUPTED_PUSH_SENTENCE`]), so `mirror sync`, `status`, `doctor`,
//! `verify` and `prune-orphans` cannot drift into five wordings of the same
//! reassurance — and none of them may call the debris corrupt or damaged.

use crate::packcheck::PackError;

/// The one public sentence for torn debris, the same on every surface that
/// words it: the mirror's rows and sync reminder, `status`, `doctor`'s debris
/// lines, `verify`'s torn line and `prune-orphans`' torn candidates. English
/// only, like all user-visible strings.
pub const INTERRUPTED_PUSH_SENTENCE: &str =
    "An interrupted push left a partial file. It belongs to no backup and can be ignored.";

/// The standing answer for "how old is this debris": the backend interface
/// carries no modification time at all, so the earliest-seen time is unknown —
/// never a fabricated clock, and never a zero that reads as "just now".
pub const NO_MTIME_SENTENCE: &str =
    "the backend interface carries no modification time, so the earliest-seen time is unknown";

/// The classification of one pack that something already flagged — no index
/// file names it, or its bytes did not read back as expected. One function
/// ([`classify_pack`]) decides it for `mirror`, `prune-orphans`, `doctor` and
/// `verify`, so two readers cannot call the same bytes two things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackClass {
    /// No index file names the pack, and it cannot be read as a pack. Debris:
    /// skip it, report it with [`INTERRUPTED_PUSH_SENTENCE`], never as a
    /// failure and never as corruption.
    Torn {
        /// The probe's own reason (header does not fit / will not decrypt), for
        /// the surfaces that print one reason per pack.
        why: String,
    },
    /// No index file names the pack, and it verifies complete. Adoptable by the
    /// next open — today's behaviour, not a failure, never torn.
    Stranded,
    /// An index file names the pack, and its bytes do not match. The hard
    /// failure it was before this module existed: exit 1.
    Corrupt {
        /// The verifier's own reason for the mismatch.
        why: String,
    },
    /// The classification could not be established: the index files could not
    /// be read, or a pack that is neither torn nor verifiable. Fail closed —
    /// no caller may skip or name torn debris from a [`PackClass::FailClosed`].
    FailClosed {
        /// What could not be established, worded for the caller's surface.
        why: String,
    },
}

/// The one classification shared by every reader that scans the repository.
///
/// `named_by_index` is `Some(bool)` once the index files were read
/// (`false` = no index file names this pack) and `None` when they could not be
/// read — the fail-closed input. `check` is what the shared pack checks said
/// about this pack's own bytes: `Ok` for a pack that verified complete (or,
/// for the mirror's header probe, one whose header reads), and the
/// [`PackError`] the verifier refusals carry. A pack whose bytes hash-check
/// but is not verified content is not an input to this function — no caller
/// asks about a pack nothing flagged.
///
/// The brief's three rules, exactly:
///
/// * **torn** — `Ok` never; `Err(PackError::UnreadableHeader)` and no index
///   names it.
/// * **stranded** — no index names it and `Ok` (verified complete).
/// * **corrupt** — an index names it and the bytes did not check.
///
/// Everything else — index unreadable, or a readable header over bytes that do
/// not verify — is [`PackClass::FailClosed`]: the caller keeps today's
/// behaviour (unknown / refused / the exit-1 hard failure) and never skips.
#[must_use]
pub fn classify_pack(named_by_index: Option<bool>, check: &Result<(), &PackError>) -> PackClass {
    let Err(failure) = check else {
        return match named_by_index {
            // Indices were read, and a pack no index file names that verifies
            // complete: content that has not been indexed yet.
            Some(false) => PackClass::Stranded,
            // An index names a pack whose bytes verify. That state does not
            // arise with the callers this function serves — each reaches it
            // holding a pack that something flagged — so fail closed rather
            // than invent a fifth class no caller words. The reason states
            // only what this arm knows: the index was read, and it names the
            // pack (W1040f — it used to assert that fact over an unreadable
            // index too).
            Some(true) => PackClass::FailClosed {
                why: "an index file names this pack and its bytes verify; there is \
                      no debris to classify"
                    .to_string(),
            },
            // The index state is unknown, so whether any index file names a
            // verifying pack cannot be established either — the same fail-closed
            // rule as below, worded for bytes that verify (W1040f: this used to
            // claim "an index file names this pack", which an unreadable index
            // does not establish).
            None => PackClass::FailClosed {
                why: "the index files could not be read, so whether any index \
                      file names this pack is unknown (its bytes verify)"
                    .to_string(),
            },
        };
    };
    match (named_by_index, failure) {
        // Fail closed, before anything else: with the index unreadable, torn
        // and stranded cannot be told apart, and a skipped pack on a guess is
        // the hole this classification exists to close.
        (None, _) => PackClass::FailClosed {
            why: format!(
                "the index files could not be read, so whether any index file \
                 names this pack is unknown ({failure})"
            ),
        },
        // An index file names these bytes: they must match, and the verifier
        // says they do not. The pre-existing hard failure, unchanged.
        (Some(true), failure) => PackClass::Corrupt {
            why: failure.to_string(),
        },
        // No index names it and it cannot be read as a pack: debris from an
        // interrupted push. Skip it; never a failure, never corruption.
        (Some(false), PackError::UnreadableHeader { why }) => PackClass::Torn { why: why.clone() },
        // No index names it, but the pack reads as a pack while not verifying
        // complete — damage in the blobs or under the name, not the shape an
        // interrupted push leaves. Unknown: today's behaviour, never torn.
        (Some(false), PackError::Unusable { why }) => PackClass::FailClosed { why: why.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unusable(reason: &str) -> PackError {
        PackError::Unusable {
            why: reason.to_string(),
        }
    }

    fn unreadable(reason: &str) -> PackError {
        PackError::UnreadableHeader {
            why: reason.to_string(),
        }
    }

    /// The brief's torn rule: unindexed + header that does not fit or will not
    /// decrypt ⇒ torn, regardless of anything else those bytes could be.
    #[test]
    fn unindexed_and_headerless_is_torn() {
        let unindexed = Some(false);
        let misses_half_the_file = unreadable("pack abc declares a 9000-byte header");
        assert!(matches!(
            classify_pack(unindexed, &Err(&misses_half_the_file)),
            PackClass::Torn { .. }
        ));
        let mac_refused =
            unreadable("pack abc: its bytes do not decrypt with this repository's key");
        assert!(matches!(
            classify_pack(unindexed, &Err(&mac_refused)),
            PackClass::Torn { .. }
        ));
        let stub = unreadable("pack abc is 30 bytes, too short to hold a header");
        assert!(matches!(
            classify_pack(unindexed, &Err(&stub)),
            PackClass::Torn { .. }
        ));
    }

    /// The brief's stranded and corrupt rules, and that neither is the other:
    /// verdict flips on the index, not on the verifier.
    #[test]
    fn the_index_decides_between_stranded_and_corrupt() {
        let bad_name = unusable("pack abc is not the bytes it is stored under");
        assert_eq!(
            classify_pack(Some(false), &Err(&bad_name)),
            PackClass::FailClosed {
                why: bad_name.to_string()
            }
        );
        assert_eq!(
            classify_pack(Some(true), &Err(&bad_name)),
            PackClass::Corrupt {
                why: bad_name.to_string()
            }
        );
        // The same bad bytes, torn-shaped, stay a hard corruption once an
        // index names them (ISSUE-006 fixture 2).
        let torn_shaped = unreadable("pack abc declares a 9000-byte header");
        assert_eq!(
            classify_pack(Some(true), &Err(&torn_shaped)),
            PackClass::Corrupt {
                why: torn_shaped.to_string()
            }
        );
        // And a complete unindexed pack is stranded (ISSUE-006 fixture 3):
        // adoptable, never reported as torn.
        assert_eq!(classify_pack(Some(false), &Ok(())), PackClass::Stranded);
    }

    /// The fail-closed rule: an unreadable index is never guessed into torn,
    /// stranded or corrupt — whatever the verifier said.
    #[test]
    fn an_unreadable_index_fails_closed_whatever_the_bytes() {
        for check in [
            Ok(()),
            Err(unreadable("pack abc declares a 9000-byte header")),
            Err(unusable("pack abc is not the bytes it is stored under")),
        ] {
            let verdict = classify_pack(None, &check.as_ref().map(|_| ()));
            assert!(
                matches!(verdict, PackClass::FailClosed { .. }),
                "an unreadable index must fail closed, got {verdict:?}"
            );
        }
    }

    /// W1040f: the two fail-closed facts that come with verifying bytes are
    /// worded as two different reasons. An index the classification could not
    /// read must not claim "an index file names this pack" — that is the
    /// fact only the read-and-named state has, and the old single reason
    /// asserted it over both.
    #[test]
    fn an_unreadable_index_never_claims_the_pack_was_named_by_one() {
        let PackClass::FailClosed { why: unknown_why } = classify_pack(None, &Ok(())) else {
            panic!("an unreadable index over verifying bytes fails closed");
        };
        let PackClass::FailClosed { why: named_why } = classify_pack(Some(true), &Ok(())) else {
            panic!("an index-named pack over verifying bytes fails closed");
        };
        assert!(
            unknown_why.contains("could not be read"),
            "the unreadable-index reason states what failed (the read): {unknown_why}"
        );
        assert!(
            !unknown_why.contains("an index file names this pack"),
            "the unreadable-index reason must not claim the naming fact: {unknown_why}"
        );
        assert!(
            named_why.contains("an index file names this pack"),
            "the named state claims its own fact: {named_why}"
        );
    }

    /// Every surface words torn debris with the same sentence, and that
    /// sentence never says corrupt, damaged or broken.
    #[test]
    fn the_public_sentence_is_one_steady_reassurance() {
        assert_eq!(
            INTERRUPTED_PUSH_SENTENCE,
            "An interrupted push left a partial file. It belongs to no backup and can be ignored."
        );
        for forbidden in ["corrupt", "damaged", "broken", "source corruption"] {
            assert!(
                !INTERRUPTED_PUSH_SENTENCE
                    .to_ascii_lowercase()
                    .contains(forbidden),
                "the torn sentence must not say {forbidden}"
            );
        }
    }
}
