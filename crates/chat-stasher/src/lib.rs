//! chat-stasher: append-only archive for LLM harness conversations.
//!
//! This crate currently exposes the CLI skeleton plus the local harness
//! scanner and the BackupStore (rustic_core wrapper). Everything is
//! deliberately read-only: no session content is ever read or printed in this
//! spike, except when `read` verifies a session back from the repository.

pub mod activity;
pub mod body_cache;
pub mod collect;
pub mod config;
pub mod credentials;
pub mod destinit;
pub mod doctor;
pub mod export;
pub mod fts;
pub mod id;
pub mod identity;
pub mod inbox;
pub mod json_out;
pub mod manifest;
pub mod metahash;
pub mod models;
pub mod nativehost;
pub mod normalize;
pub mod overview;
pub mod push_progress;
pub mod readback;
pub mod reap;
pub mod remote_err;
pub mod runstate;
pub mod scanner;
pub mod schedule;
pub mod seal;
pub mod search;
pub mod selector;
pub mod sidecar;
pub mod sqlite_probe;
pub mod stagereclaim;
pub mod store;
pub mod ui;
pub mod verify;
pub mod view;
