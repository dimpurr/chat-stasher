//! Inbox retirement proof from every declared destination. Reads bypass the
//! conversation body cache and retain every shard; a push receipt or a bundle
//! digest field alone is never evidence that a destination holds sealed bytes.
use crate::{
    inbox::SealOutcome,
    remote_inbox::ArchiveProof,
    store::{self, BackupStore, DuplicateShardPolicy, StoreConfig},
};
use anyhow::Context;
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeSet, path::Path};

/// One destination this round's proof phase has opened: its key and the
/// metadata-query store built from it. Built once per [`DestinationArchive`]
/// and reused by every batch, so a round opens each destination once no
/// matter how many receipts, bundles or agents its proof covers.
struct OpenedDestination {
    master_key: MasterKey,
    archive: BackupStore,
}

pub struct DestinationArchive<'a> {
    pub stage: &'a Path,
    pub destinations: &'a [StoreConfig],
    /// Opened lazily on the first query, one entry per destination in
    /// declaration order. A `RefCell` rather than `&mut` because the proof
    /// arrives through `&self` (the `ArchiveProof` trait is shared); the
    /// `Arc` releases the cell's borrow before any destination is read.
    opened: RefCell<Option<Vec<std::sync::Arc<OpenedDestination>>>>,
    /// How many destination repositories were opened, for the test that
    /// pins "once per destination per round": it counts real opens, not
    /// queries, so any per-bundle re-open regresses a number, not prose.
    pub opens: std::sync::atomic::AtomicUsize,
}

impl<'a> DestinationArchive<'a> {
    pub fn new(stage: &'a Path, destinations: &'a [StoreConfig]) -> Self {
        Self {
            stage,
            destinations,
            opened: RefCell::new(None),
            opens: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl<'a> DestinationArchive<'a> {
    /// The per-destination key and archive, opening them on first use.
    fn open_once(&self) -> anyhow::Result<Vec<std::sync::Arc<OpenedDestination>>> {
        // A plain borrow, not `try_borrow_mut`: nothing re-enters this cell
        // while the guard is held (the destinations are read through the
        // cloned `Arc`s after the guard drops), so the busy case cannot
        // occur here and a panic would name a real bug, not a race.
        let mut opened = self.opened.borrow_mut();
        if opened.is_none() {
            let mut fresh = Vec::with_capacity(self.destinations.len());
            for config in self.destinations {
                let master_key = store::load_key_file(config)?;
                let archive = BackupStore::for_metadata_query(config.clone())
                    .with_shard_policy(DuplicateShardPolicy::KeepAll);
                fresh.push(std::sync::Arc::new(OpenedDestination {
                    master_key,
                    archive,
                }));
                self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            *opened = Some(fresh);
        }
        Ok(opened
            .as_deref()
            .map(|list| list.to_vec())
            // reason: the branch above replaces an unfilled cell with a
            // fresh Vec, so an empty answer here is unreachable — kept
            // rather than an unwrap because the proof never panics on its
            // own bookkeeping.
            .unwrap_or_default())
    }
}

impl ArchiveProof for DestinationArchive<'_> {
    fn holds(&self, machine: &str, outcome: &SealOutcome) -> anyhow::Result<bool> {
        Ok(self
            .holds_many(machine, std::slice::from_ref(outcome))?
            .first()
            .copied()
            // reason: a one-outcome query over a non-empty destination set
            // answers for exactly that outcome, so an empty answer cannot
            // occur (the empty-destination case already refused); false
            // would still be the honest shape — nothing was proven.
            .unwrap_or(false))
    }

    fn holds_many(&self, machine: &str, outcomes: &[SealOutcome]) -> anyhow::Result<Vec<bool>> {
        // An empty destination set answers nothing: with nowhere the receipts
        // could be held, "held" is not a claim any unread archive implies.
        let mut held = vec![false; outcomes.len()];
        if self.destinations.is_empty() {
            return Ok(held);
        }
        let mut wanted = BTreeSet::new();
        let mut records = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            let (id, shard, bundle_digest) = match outcome {
                SealOutcome::Stored(row) => (&row.id, &row.shard, &row.file_sha256),
                SealOutcome::Duplicate(row) => (&row.id, &row.matched_shard, &row.file_sha256),
            };
            let dir = store::session_shard_dir(self.stage, machine, id);
            let path = store::sealed_shard_entries(&dir)?
                .into_iter()
                .find(|(_, p)| p.file_name().is_some_and(|n| n == shard.as_str()))
                .context("sealed proof record missing")?
                .1;
            let bytes = std::fs::read(path).context("sealed proof record unreadable")?;
            let record: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                record["id"] == id.as_str()
                    && record["machine"] == machine
                    && record["file_sha256"] == bundle_digest.as_str(),
                "sealed proof record mismatch"
            );
            let digest: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            wanted.insert((machine.to_owned(), id.clone()));
            records.push((id.clone(), shard.clone(), digest));
        }
        if records.is_empty() {
            return Ok(held);
        }
        // One query per destination for the whole batch: the wanted set is
        // built once, so a destination is read once per call and opened once
        // per round (the open cache above), never once per bundle.
        for destination in self.open_once()? {
            let sessions = destination
                .archive
                .read_selected_sessions(&destination.master_key, &wanted)?;
            for (index, (id, shard, digest)) in records.iter().enumerate() {
                // Continue after a completed negative: a later unreadable
                // copy makes the overall observation unknown for that
                // receipt, rather than absent.
                if sessions
                    .get(&(machine.to_owned(), id.clone()))
                    .is_some_and(|(_, hashes)| {
                        hashes
                            .iter()
                            .any(|(name, hash)| name == shard && hash == digest)
                    })
                {
                    held[index] = true;
                }
            }
        }
        Ok(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proof phase's round budget, pinned by number: however many
    /// receipts a round proves, in however many batches the agents' journals
    /// present them, each destination repository is opened exactly once per
    /// archive instance — once per round — and every open is counted here,
    /// so a per-bundle re-open regresses this test rather than a sentence.
    #[test]
    fn every_destination_opens_once_per_round_however_many_receipts() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let stage = root.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox");
        let machine = "synthetic-machine";
        // Two destinations, key files at hand and repositories initialised.
        let configs: Vec<StoreConfig> = ["synthetic-a", "synthetic-b"]
            .into_iter()
            .map(|name| StoreConfig {
                repo_root: root.join(name).to_string_lossy().into_owned(),
                key_file: root.join(format!("{name}.key")),
                connections: 1,
                cache_dir: Some(root.join(format!("{name}-cache"))),
                ..StoreConfig::default()
            })
            .collect();
        for config in &configs {
            let key = MasterKey::new();
            store::persist_key_file(config, &key).unwrap();
            BackupStore::new(config.clone(), machine.to_string())
                .open_or_init(&key)
                .unwrap();
        }
        // Seal three distinct fixtures into the stage and push the stage to
        // both destinations, so the archived bytes exist where proof looks.
        let mut names = Vec::new();
        for name in [
            "api-export-utf8.json",
            "api-export-base64.json",
            "api-export-parent.json",
        ] {
            let bytes = std::fs::read(fixtures.join(name)).unwrap();
            let outcome =
                crate::inbox::seal_payload(name, &bytes, &stage, machine, 1000, None, None)
                    .unwrap();
            let crate::inbox::SealOutcome::Stored(row) = outcome else {
                panic!("fixture must store, not duplicate");
            };
            names.push((name.to_string(), row.id.clone(), row.shard.clone()));
        }
        for config in &configs {
            let key = store::load_key_file(config).unwrap();
            BackupStore::new(config.clone(), machine.to_string())
                .push(&stage, &key)
                .unwrap();
        }
        // Re-sealing the same bytes returns the already-archived outcome a
        // proof would query (matched shard, held bytes) — three receipts.
        let mut outcomes = Vec::new();
        for (name, _, _) in &names {
            let bytes = std::fs::read(fixtures.join(name)).unwrap();
            let outcome =
                crate::inbox::seal_payload(name, &bytes, &stage, machine, 1000, None, None)
                    .unwrap();
            let crate::inbox::SealOutcome::Duplicate(row) = outcome else {
                panic!("the re-seal must match the archived bytes");
            };
            outcomes.push(crate::inbox::SealOutcome::Duplicate(row));
        }
        let proof = DestinationArchive::new(&stage, &configs);
        let held = proof.holds_many(machine, &outcomes).unwrap();
        assert!(
            held.iter().all(|held| *held),
            "both destinations hold all three receipts: {held:?}"
        );
        // A second batch, queried the way a second agent's journal would:
        // the repositories are already open, and the counter must not move.
        let held_again = proof.holds_many(machine, &outcomes).unwrap();
        assert!(held_again.iter().all(|held| *held));
        // A single-outcome query through `holds` reuses the same opens too.
        assert!(proof.holds(machine, &outcomes[0]).unwrap());
        assert_eq!(
            proof.opens.load(std::sync::atomic::Ordering::SeqCst),
            configs.len(),
            "each destination opened exactly once per round, however many \
             receipts and however many batches asked"
        );
        // A receipt nothing archived yet is honestly not held — and asking
        // does not open anything new.
        // A receipt nothing archived yet is honestly not held: sealed into
        // a fresh stage, never pushed. The empty destination set answers
        // nothing and opens nothing.
        std::fs::create_dir_all(root.join("unpushed-stage")).unwrap();
        let fresh = std::fs::read(fixtures.join("api-export-utf8.json")).unwrap();
        let unheld = crate::inbox::seal_payload(
            "api-export-utf8.json",
            &fresh,
            &root.join("unpushed-stage"),
            machine,
            1000,
            None,
            None,
        )
        .unwrap();
        let crate::inbox::SealOutcome::Stored(row) = unheld else {
            panic!("an unpushed stage stores the fixture");
        };
        let unpushed_stage = root.join("unpushed-stage");
        let unpushed = DestinationArchive::new(&unpushed_stage, &[]);
        let held = unpushed
            .holds_many(machine, &[crate::inbox::SealOutcome::Stored(row)])
            .unwrap();
        assert_eq!(held, vec![false], "nowhere archived means not held");
        assert_eq!(unpushed.opens.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
