//! Keep a reading of the archive from becoming a non-answer.
//!
//! The pinned reader (`rustic_core` 0.12.0) responds to a **truncated data
//! pack** in two ways that are both worse than an error, and both are reachable
//! from our own calls:
//!
//! * **It crashes.** `CachedBackend::read_partial` (`src/backend/cache.rs`)
//!   serves a cacheable blob by reading the whole file from the backend and then
//!   slicing it to the range the index recorded — without checking that the file
//!   is that long. A tree blob is cacheable even though the *pack* is not
//!   (`BlobType::Tree::is_cacheable()` vs `FileType::Pack::is_cacheable()`), so
//!   a tree in a truncated pack slices past the end of the buffer and
//!   `Bytes::slice` panics: `range end out of bounds: 3012 <= 1749`.
//! * **It deadlocks.** `TreeStreamerOnce::new` (`src/blob/tree.rs`) loads trees
//!   on detached worker threads and hands results back over a crossbeam
//!   channel. A worker that panics dies without sending and without closing
//!   anything — the other workers are idle on `in_rx`, which the streamer still
//!   holds — so the consumer blocks in `recv()` forever. `check` reaches this
//!   before it returns the pack-size finding it has already collected, so
//!   `verify` hangs rather than reporting the damage it found.
//!
//! `CLAUDE.md` invariant 2 makes both unacceptable: `3` means "did not finish
//! reading", and a crash (`101`) and a hang are neither a diagnosis nor a
//! measurement — the first reads as a broken tool, the second as a stuck one,
//! and neither tells the user their archive is unreadable.
//!
//! # The fix, the fallback, and the refusal
//!
//! 1. **The fix.** [`BackupStore::require_sound_packs`] asks the question the
//!    reader never asks itself: *is any pack the index references shorter than
//!    the index says it must be?* The index answers it without reading a byte of
//!    any pack, so a truncated archive is refused before the code that
//!    mishandles it runs.
//! 2. **The fallback.** [`catching_panic`] runs the reader call on a thread this
//!    module owns and joins, so a panic on the reader's own thread comes back as
//!    the `Err` channel its callers already map to exit 3 instead of crashing.
//! 3. **The refusal.** No panic hook is installed. See below.
//!
//! # Why no panic hook
//!
//! The deadlock above is the case a `catch_unwind` cannot reach: the panic is on
//! a rustic worker and our thread is blocked, so the hook is the only code that
//! runs. An earlier revision of this module installed one, and it ended the
//! process with exit 3 for a panic on *any* thread other than the caller's — a
//! hook cannot tell `TreeStreamerOnce`'s workers from any other thread, because
//! `rustic_core` spawns them detached and unnamed. That made an unrelated panic
//! — a parallel test, another library's background thread — refuse a **healthy**
//! archive as unreadable, which is the one outcome this module exists to
//! prevent: a wrong "did not finish" is not safer than no answer, it is the
//! wrong answer wearing the right exit code.
//!
//! So the hook is gone rather than narrowed, and this module states the residual
//! plainly instead of trading a healthy-archive misreport for it: if a panic
//! `require_sound_packs` did not foresee lands on a rustic worker thread, the
//! reader can still block on the dead worker's channel. Nothing reachable from
//! the reader's own thread can break that block, and the only code that can
//! (a process-wide hook) cannot tell whose panic it is looking at.
//!
//! # Why this is not a timeout
//!
//! A wall-clock limit on `verify` was rejected deliberately. It cannot tell a
//! wedged reader from a slow remote, and this project verifies archives over
//! SFTP, where a slow read is normal — so the only limit that would never cut a
//! legitimate run is one far above every legitimate run, which is not a bound.
//! A panic, unlike a slow read, is never a legitimate part of one, so the
//! fallback keys on it and cannot cut a remote that is merely slow.

use anyhow::Context;
use std::collections::BTreeMap;

use rustic_core::repofile::{IndexFile, IndexId};
use rustic_core::{FileType, Open, ReadBackend, Repository};

use crate::store::BackupStore;

/// A pack whose file is present but shorter than the index records.
///
/// *Present but shorter* is the whole of it. A **missing** pack is not this
/// type, because a missing file is a different failure with a different
/// behaviour — see [`BackupStore::audit_pack_sizes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedPack {
    /// The pack's id, hex.
    pub pack: String,
    /// Bytes the index records for the pack: its own header plus every blob.
    pub recorded: u64,
    /// Bytes actually present.
    pub present: u64,
}

/// Every live pack the index references, checked against the bytes present.
///
/// `packs` is the denominator, so a caller can say "1 of 2" and not just "1".
#[derive(Debug, Clone, Default)]
pub struct PackSizeAudit {
    /// Live packs the index references.
    ///
    /// Packs already marked for deletion are **not** counted: nothing reads
    /// them, so their size is not this question, and refusing an archive over
    /// one would refuse an archive that reads perfectly.
    pub packs: usize,
    /// The packs that are present and shorter than the index records. Sorted by
    /// pack id, so the message is stable across runs. Empty means every
    /// referenced pack is long enough for what it must contain.
    pub truncated: Vec<TruncatedPack>,
}

impl PackSizeAudit {
    /// True when no referenced pack is truncated.
    pub fn sound(&self) -> bool {
        self.truncated.is_empty()
    }
}

impl BackupStore {
    /// Every live pack the index references that is **present but truncated**.
    ///
    /// The recorded size is taken from the index, which is the same authority
    /// `rustic`'s own `check_packs_list` uses (it compares the index's
    /// `IndexPack::pack_size()` against the backend's listing and reports any
    /// difference). Reading it here means the answer is decided before a single
    /// pack byte is fetched, which matters because the fetch is what panics.
    ///
    /// Read through `repo`, not through a raw backend, on purpose: `repo` is the
    /// view the reader itself will use, so if the metadata cache is serving an
    /// index, this audit judges the archive by the same index the reader will
    /// judge it by. An audit that disagreed with the reader would be worse than
    /// none.
    ///
    /// # What is deliberately not a finding
    ///
    /// * A pack **absent** from the backend. Its read returns `Err`, not a short
    ///   buffer, so nothing downstream slices anything and nothing wedges:
    ///   `rustic`'s own `check` reports it as a finding — which `verify` is meant
    ///   to pass on, and does (`verify_test::l1_catches_a_missing_pack`) — while
    ///   `read` fails cleanly with exit 3. Refusing it here would buy no safety
    ///   and would cost `verify` the finding it exists to produce.
    /// * A pack **longer** than the index records. That is odd, not unreadable;
    ///   the only hazard guarded here is a read running off the end of a file.
    pub fn audit_pack_sizes<S: Open>(&self, repo: &Repository<S>) -> anyhow::Result<PackSizeAudit> {
        let present: BTreeMap<String, u64> = self
            .backends()?
            .repository()
            .list_with_size(FileType::Pack)
            .context("list data packs for the size audit")?
            .into_iter()
            .map(|(id, size)| (id.to_hex().as_str().to_string(), u64::from(size)))
            .collect();

        let mut audit = PackSizeAudit::default();
        for index_id in repo.list::<IndexId>().context("list index files")? {
            let index: IndexFile = repo
                .get_file(&index_id)
                .context("read an index file for the size audit")?;
            for pack in &index.packs {
                audit.packs += 1;
                let recorded = u64::from(pack.pack_size());
                let pack_id = pack.id.to_hex().as_str().to_string();
                match present.get(&pack_id).copied() {
                    Some(size) if size < recorded => audit.truncated.push(TruncatedPack {
                        pack: pack_id,
                        recorded,
                        present: size,
                    }),
                    // Absent or not shorter: not this check's question. See the
                    // doc comment for why absence is left to `rustic`'s check.
                    _ => {}
                }
            }
        }
        audit.truncated.sort_by(|a, b| a.pack.cmp(&b.pack));
        Ok(audit)
    }

    /// Refuse an archive that holds a pack too short for what the index records.
    ///
    /// `Ok(())` means no live pack is truncated, so the reader `rustic` is about
    /// to run has inputs it cannot run off the end of. The failure is
    /// deliberately "unreadable", which the CLI reports as exit 3 — never as an
    /// empty or partial archive, because a caller's "there is nothing here" must
    /// not be built on a read that did not happen.
    pub fn require_sound_packs<S: Open>(&self, repo: &Repository<S>) -> anyhow::Result<()> {
        let audit = self.audit_pack_sizes(repo)?;
        let Some(first) = audit.truncated.first() else {
            return Ok(());
        };
        anyhow::bail!(
            "this archive cannot be read: {} of {} pack(s) the index references is shorter than \
             the index records (pack {}: the index records {} bytes, {} are present). Nothing was \
             read, so nothing below is a count of what the archive holds",
            audit.truncated.len(),
            audit.packs,
            first.pack,
            first.recorded,
            first.present,
        )
    }
}

/// Run a reader call on a thread this guard owns, so a panic comes back as an
/// `Err` instead of a crash.
///
/// The call runs on a thread this function spawns and joins. That is the whole
/// of the guard's reach: a panic inside foreign code is caught by that thread's
/// join, so it never unwinds through the caller's own frames, and the caller
/// keeps whatever stack it had. `std::thread::scope` joins before returning, so
/// the borrows the call needs (`&BackupStore`, `&MasterKey`) stay valid without
/// being `'static`, and no panic hook is installed or restored — the process's
/// hook is exactly the one it had. See the module docs for why an earlier
/// revision installed one and why that was wrong.
///
/// The returned error lands on the channel every reader call already has, so
/// each caller's existing "could not read it" arm — the one that exits 3 — keeps
/// working with no new branch. `what` names the operation for the note.
///
/// A panic on a thread `rustic_core` spawned for itself does not come back this
/// way; the module docs say why nothing here can attribute one.
pub fn catching_panic<T: Send>(
    what: &str,
    f: impl FnOnce() -> anyhow::Result<T> + Send,
) -> anyhow::Result<T> {
    let outcome = std::thread::scope(|scope| scope.spawn(f).join());

    match outcome {
        Ok(result) => result,
        Err(payload) => Err(anyhow::anyhow!(
            "{what}: this read cannot finish — the reader panicked: {}. This does not say the \
             archive is empty; it says it was not read",
            payload_note(payload.as_ref()),
        )),
    }
}

/// The message a panic payload carries, or an honest statement that it carries
/// none — never an invented one.
fn payload_note(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic whose payload carries no message".to_string()
    }
}

#[cfg(test)]
mod tests {
    // The one property these tests cannot pin from in here — that a guarded
    // call leaves the process's panic hook exactly as it found it — is pinned in
    // `tests/w244_panic_hook_test.rs`. It is a file of its own on purpose:
    // cargo gives every file under `tests/` its own process, so a hook that test
    // installs reaches no other test, and the hook can be handed back exactly
    // rather than wrapped. A `#[cfg(test)]` test here shares this binary's
    // process with every test in it.
    use super::*;

    #[test]
    fn a_panic_in_the_reader_comes_back_as_a_note() {
        let outcome: anyhow::Result<()> = catching_panic("read", || panic!("boom: truncated pack"));
        let err = outcome.expect_err("a panicking reader must not return Ok");
        let text = err.to_string();
        assert!(
            text.contains("boom: truncated pack"),
            "the panic's own message must survive: {text}"
        );
        assert!(
            text.contains("not read"),
            "the note must say the archive was not read, never that it was empty: {text}"
        );
    }

    /// A panic on another thread, while a read is in flight, must not be
    /// attributed to that read.
    ///
    /// This is the property the guard's first revision got wrong: it installed a
    /// process-wide hook that ended the process with exit 3 for a panic on any
    /// thread other than the caller's, so a panic elsewhere in the process — a
    /// parallel test, another library's background thread — refused a healthy
    /// archive as unreadable. The read below is healthy and returns its own
    /// result; the other thread's panic is neither attributed to it nor
    /// suppressed, and the process is still running to say so.
    #[test]
    fn an_unrelated_panic_on_another_thread_is_not_attributed_to_the_read() {
        let unrelated = std::thread::spawn(|| panic!("an unrelated thread's panic"));
        let outcome = catching_panic("read", || {
            // Joined inside the read's window on purpose: the other thread has
            // finished unwinding before this call returns, so the panic cannot
            // be waved off as one that happened after the read was over.
            assert!(
                unrelated.join().is_err(),
                "the other thread was supposed to panic"
            );
            Ok(7)
        });
        assert_eq!(
            outcome.expect("a panic this read did not cause must not fail it"),
            7
        );
    }

    #[test]
    fn a_finished_call_passes_its_result_through_unchanged() {
        let outcome = catching_panic("read", || Ok(41 + 1));
        assert_eq!(outcome.expect("no panic happened"), 42);
    }

    #[test]
    fn an_error_from_the_call_is_not_replaced_by_the_guard() {
        let outcome: anyhow::Result<()> =
            catching_panic("read", || anyhow::bail!("the archive said no"));
        let text = outcome.expect_err("the error must propagate").to_string();
        assert_eq!(text, "the archive said no");
    }

    #[test]
    fn a_panic_payload_without_a_message_is_reported_as_such() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(7u32);
        assert_eq!(
            payload_note(payload.as_ref()),
            "a panic whose payload carries no message"
        );
        let text: Box<dyn std::any::Any + Send> = Box::new("borrowed text");
        assert_eq!(payload_note(text.as_ref()), "borrowed text");
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned text"));
        assert_eq!(payload_note(owned.as_ref()), "owned text");
    }

    #[test]
    fn a_sound_audit_needs_no_truncated_packs() {
        assert!(PackSizeAudit::default().sound());
        assert!(!PackSizeAudit {
            packs: 1,
            truncated: vec![TruncatedPack {
                pack: "aa".into(),
                recorded: 10,
                present: 5,
            }],
        }
        .sound());
    }
}
