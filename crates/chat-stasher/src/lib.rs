//! chat-stasher: append-only archive for LLM harness conversations.
//!
//! This crate currently exposes the CLI skeleton plus the local harness
//! scanner and the BackupStore (rustic_core wrapper). Everything is
//! deliberately read-only: no session content is ever read or printed in this
//! spike, except when `read` verifies a session back from the repository.

pub mod activity;
pub mod audit_store;
pub mod body_cache;
pub mod bundle_transport;
pub mod collect;
pub mod config;
pub mod credentials;
pub mod destinit;
pub mod doctor;
pub mod export;
pub mod fts;
pub mod grok_bot;
pub mod id;
pub mod identity;
pub mod inbox;
pub mod inbox_config;
pub mod json_out;
pub mod keydecl;
pub mod manifest;
pub mod message_audit;
pub mod metahash;
pub mod models;
pub mod nativehost;
pub mod normalize;
pub mod orphans;
pub mod overview;
pub mod packcheck;
pub mod provenance;
pub mod prune_orphans;
pub mod push_progress;
pub mod readback;
pub mod reader_guard;
pub mod reap;
pub mod remote_err;
pub mod remote_inbox;
pub mod runstate;
pub mod scanner;
pub mod schedule;
pub mod seal;
pub mod search;
pub mod selector;
pub mod send;
pub mod send_key;
mod shard_writer;
pub mod sidecar;
pub mod snapshot_cache;
pub mod sqlite_probe;
pub mod stagereclaim;
pub mod store;
pub mod test_identity_guard;
#[cfg(test)]
mod test_support;
pub mod ui;
pub mod verify;
pub mod view;
