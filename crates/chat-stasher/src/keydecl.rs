//! Which archive keys the user has said they keep a copy of, kept durable.
//!
//! Every archive copy has its own key: the local repository's `masterkey.json`
//! and each destination's `masterkey-<name>.json` (ADR-013/ADR-039). A second
//! machine reads a destination using that destination's key, so a user who
//! backs up only the local key cannot read their off-site copy after a loss.
//! The setup wizard therefore asks the user to confirm they saved *every* key,
//! and this module records which scopes they said they did.
//!
//! A record here is a *statement by the user*, never a verification: the tool
//! cannot see the copy, so `declaration_is_verified` is always `false`. Absence
//! of a record is the safe default ("not declared"); a present record for a key
//! that no longer exists, or a malformed file, never reads as "declared".
//!
//! `status`/`doctor` read this file to say which destination keys exist locally
//! and which the user has declared saved.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name inside `collect::default_state_dir()`.
pub const KEY_DECLARATIONS_FILE: &str = "key-declarations.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version is
/// treated as absent, never as declared.
pub const KEY_DECLARATIONS_VERSION: u32 = 1;

/// The scope id of the local repository's key.
pub const LOCAL_SCOPE: &str = "local";

/// The scope id of one destination's key.
///
/// Prefixed rather than the bare destination name so a destination a user
/// happens to call `local` cannot be recorded as — or read back as — the local
/// repository's key. A collision there would report a declared backup for a key
/// whose copy nobody confirmed, which is the one direction this file must never
/// get wrong.
pub fn destination_scope(name: &str) -> String {
    format!("destination:{name}")
}

/// One recorded declaration: the user said they keep a copy of this key file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyDeclaration {
    /// The key file this statement is about. Recorded so a reader can see which
    /// path the user was looking at; a path is not key material.
    pub path: PathBuf,
    /// Wall-clock seconds since the epoch when the statement was made.
    pub declared_at_unix: u64,
    /// Always `false`: nothing here verifies that a copy exists.
    #[serde(default)]
    pub declaration_is_verified: bool,
}

/// Scope id ([`LOCAL_SCOPE`] or [`destination_scope`]) -> declaration.
pub type Declarations = BTreeMap<String, KeyDeclaration>;

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    declarations: Declarations,
}

pub fn key_declarations_path(state_dir: &Path) -> PathBuf {
    state_dir.join(KEY_DECLARATIONS_FILE)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        // reason: a pre-1970 clock is not reachable on a machine that can run
        // this, and 0 is a harmless timestamp for a record written just now —
        // the declaration is never gated on its time.
        .unwrap_or(0)
}

/// Read the recorded declarations. Missing, malformed, or an unknown version
/// all read as empty ("nothing declared"), never as "declared": the safe
/// under-report is the one the exit-code invariants demand.
pub fn load(state_dir: &Path) -> Declarations {
    let path = key_declarations_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Declarations::new(),
        Err(_) => Declarations::new(),
        Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
            Ok(file) if file.version == KEY_DECLARATIONS_VERSION => file.declarations,
            Ok(_) | Err(_) => Declarations::new(),
        },
    }
}

/// Record that the user declared they keep a copy of `path` for `scope`.
///
/// Crash-safe: temp file + fsync + atomic rename (same shape as `runstate`).
/// Merges with what is already recorded, so declaring one destination does not
/// drop another's record. A failure to persist means the scope is *not*
/// recorded — the caller must treat it as "not declared", never claim the run
/// succeeded at declaring.
pub fn mark_declared(state_dir: &Path, scope: &str, path: &Path) -> anyhow::Result<()> {
    let mut declarations = load(state_dir);
    declarations.insert(
        scope.to_string(),
        KeyDeclaration {
            path: path.to_path_buf(),
            declared_at_unix: now_unix(),
            declaration_is_verified: false,
        },
    );
    let file = File {
        version: KEY_DECLARATIONS_VERSION,
        declarations,
    };
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = key_declarations_path(state_dir);
    let tmp = path.with_file_name(format!(".{KEY_DECLARATIONS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(&file).context("serialise key declarations")?;
    let mut handle = fs::File::create(&tmp)?;
    handle.write_all(&bytes)?;
    handle.sync_all()?;
    drop(handle);
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_empty_not_declared() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn mark_declared_round_trips_and_is_unverified() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = dir.path().join("masterkey-backup.json");
        mark_declared(dir.path(), "backup", &key).expect("persist");
        let decls = load(dir.path());
        let entry = decls.get("backup").expect("the scope is recorded");
        assert_eq!(entry.path, key);
        assert!(!entry.declaration_is_verified, "never verified");
    }

    /// A destination a user calls `local` must not be recorded as the local
    /// repository's key. The two are different copies with different files, and
    /// reading one's declaration as the other's would report a backup for a key
    /// nobody confirmed.
    #[test]
    fn a_destination_named_local_is_not_the_local_scope() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("masterkey-local.json");
        mark_declared(dir.path(), &destination_scope("local"), &dest).expect("persist");
        let decls = load(dir.path());
        assert!(
            !decls.contains_key(LOCAL_SCOPE),
            "declaring the `local` destination declared the local repository's key too"
        );
        assert_eq!(
            decls
                .get(&destination_scope("local"))
                .expect("the destination's own record")
                .path,
            dest
        );
    }

    #[test]
    fn declaring_one_scope_does_not_drop_another() {
        let dir = tempfile::tempdir().expect("temp dir");
        mark_declared(dir.path(), "local", &dir.path().join("masterkey.json")).expect("a");
        mark_declared(
            dir.path(),
            "backup",
            &dir.path().join("masterkey-backup.json"),
        )
        .expect("b");
        let decls = load(dir.path());
        assert_eq!(decls.len(), 2, "both scopes survive a merge");
    }

    #[test]
    fn a_parse_failure_reads_as_not_declared() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = key_declarations_path(dir.path());
        fs::create_dir_all(dir.path()).expect("state dir");
        fs::write(&path, b"not json at all").expect("corrupt file");
        assert!(
            load(dir.path()).is_empty(),
            "corrupt is absent, never declared"
        );
    }

    #[test]
    fn an_unknown_version_reads_as_not_declared() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = key_declarations_path(dir.path());
        fs::create_dir_all(dir.path()).expect("state dir");
        let file = File {
            version: 999,
            declarations: {
                let mut m = Declarations::new();
                m.insert(
                    "local".to_string(),
                    KeyDeclaration {
                        path: dir.path().join("masterkey.json"),
                        declared_at_unix: 0,
                        declaration_is_verified: false,
                    },
                );
                m
            },
        };
        fs::write(&path, serde_json::to_vec_pretty(&file).expect("serialise")).expect("write");
        assert!(
            load(dir.path()).is_empty(),
            "an unknown version is not declared"
        );
    }
}
