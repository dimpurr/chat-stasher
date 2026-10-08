//! Inbox retirement proof from every declared destination. Reads bypass the
//! conversation body cache and retain every shard; a push receipt or a bundle
//! digest field alone is never evidence that a destination holds sealed bytes.
use crate::{
    inbox::SealOutcome,
    remote_inbox::ArchiveProof,
    store::{self, BackupStore, DuplicateShardPolicy, StoreConfig},
};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

pub struct DestinationArchive<'a> {
    pub stage: &'a Path,
    pub destinations: &'a [StoreConfig],
}

impl ArchiveProof for DestinationArchive<'_> {
    fn holds(&self, machine: &str, outcome: &SealOutcome) -> anyhow::Result<bool> {
        if self.destinations.is_empty() {
            return Ok(false);
        }
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
        let key = (machine.to_owned(), id.clone());
        let wanted = BTreeSet::from([key.clone()]);
        let mut proven = true;
        for cfg in self.destinations {
            let master_key = store::load_key_file(cfg)?;
            let archive = BackupStore::for_metadata_query(cfg.clone())
                .with_shard_policy(DuplicateShardPolicy::KeepAll);
            let sessions = archive.read_selected_sessions(&master_key, &wanted)?;
            // Continue after a completed negative: a later unreadable copy
            // makes the overall observation unknown, rather than absent.
            proven &= sessions.get(&key).is_some_and(|(_, hashes)| {
                hashes
                    .iter()
                    .any(|(name, hash)| name == shard && hash == &digest)
            });
        }
        Ok(proven)
    }
}
