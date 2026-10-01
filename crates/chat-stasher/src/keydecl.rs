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
//! A record is about a **file**, not about a name: it carries the path the user
//! was shown and the fingerprint of the bytes that were there (see
//! [`declared_for`]). A key re-created at the same path is a different key, and
//! the declaration made about the old one must not be read as a statement about
//! it. A file that *cannot be read* — something else sitting at that path, or
//! permission to read it gone — is a third case, and it is reported as
//! [`DeclaredFor::Unreadable`]: an unknown, never a "saved" and never a plain
//! "not declared".
//!
//! `status`/`doctor` read this file to say which destination keys exist locally
//! and which the user has declared saved.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name inside `collect::default_state_dir()`.
pub const KEY_DECLARATIONS_FILE: &str = "key-declarations.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version is
/// treated as absent, never as declared.
///
/// `2` adds the fingerprint, which is what makes a record a statement about a
/// *file* rather than about a path. A version-1 file is read as absent, so a
/// record made by a build without fingerprints is re-asked rather than trusted
/// about whatever now sits at that path.
pub const KEY_DECLARATIONS_VERSION: u32 = 2;

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
    /// SHA-256 (hex) of the key file's bytes when the statement was made. A
    /// path can hold a different key tomorrow — the file deleted and re-created
    /// is a new key — and a declaration about the old bytes says nothing about
    /// the new ones.
    pub fingerprint: String,
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

/// The SHA-256 of a key file's bytes, hex — `None` when it cannot be read.
///
/// Not key material: a digest cannot be turned back into the key, and it is
/// only ever compared with another digest the same way.
pub fn fingerprint_of(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(fingerprint_bytes(&bytes))
}

/// The comparison half of [`fingerprint_of`], on bytes the caller already read.
fn fingerprint_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The answer to [`declared_for`], in three states because the reporting rules
/// split them: "the bytes matched", "we looked and they differed" and "we
/// could not look" are three different facts, and the user must never be shown
/// one of them as another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredFor {
    /// The recorded declaration is about this file: the path is the recorded
    /// one and the bytes there fingerprint to the recorded fingerprint.
    Declared,
    /// A *known* "no": nothing is recorded for the scope, the record is about
    /// another path, or the bytes at the recorded path changed since the
    /// statement was made. A key deleted and re-created at the same path is a
    /// different key, and the commonest way to get one — re-running the wizard
    /// after losing the last copy — is exactly when a false "saved" costs the
    /// most.
    NotDeclared,
    /// The recorded path matches, but the file could not be read to compare
    /// fingerprints — something unreadable sits at that path (a directory
    /// where the key was, permission to read it gone, corruption). This is an
    /// *unknown*: the file may or may not be the declared one, and reporting
    /// it as [`DeclaredFor::Declared`] would claim a confirmed backup of a
    /// file nobody has opened. It must not be worded as
    /// [`DeclaredFor::NotDeclared`] either — a "no" says the bytes were seen
    /// and did not match, and here nothing was seen at all.
    Unreadable,
}

impl DeclaredFor {
    /// Whether a declaration covers the file asked about — `true` only for a
    /// known match.
    ///
    /// For the callers that need one bit ("is the step owed?", "may the run
    /// proceed?") every other state reads as `false`, because owing the user a
    /// repeat question on an unreadable key errs safely, while reading it as
    /// saved errs in the one direction this module exists to prevent.
    /// Reporting surfaces should carry the three states instead, so an
    /// unreadable key is never shown as a plain "not declared".
    pub fn is_declared(self) -> bool {
        matches!(self, DeclaredFor::Declared)
    }
}

/// Whether these declarations say the user keeps a copy of **`path` as it is
/// now**, as a [`DeclaredFor`].
///
/// Both halves of a "yes" are required, and each catches a different false
/// "saved":
/// * the recorded path must be the path being asked about, because a
///   destination's `key_file` can change in the config and a declaration about
///   the old file says nothing about the new one;
/// * the bytes there must fingerprint to what was recorded, because a key file
///   deleted and re-created at the same path is a *different key*.
///
/// A **missing** file is [`DeclaredFor::Declared`]: `NotFound` is the one read
/// error that is an absence rather than a failure, and the statement survives
/// it — it was made about the copy the user keeps, and deleting this machine's
/// file does not unmake it. Whether the file is here is reported separately,
/// so nothing is conflated. Every other read failure is
/// [`DeclaredFor::Unreadable`] and never a "yes": a file that exists but
/// cannot be read is not the confirmed file the user's declaration describes.
pub fn declared_for(declarations: &Declarations, scope: &str, path: &Path) -> DeclaredFor {
    let Some(entry) = declarations.get(scope) else {
        return DeclaredFor::NotDeclared;
    };
    if entry.path != path {
        return DeclaredFor::NotDeclared;
    }
    match fs::read(path) {
        Ok(bytes) => {
            if fingerprint_bytes(&bytes) == entry.fingerprint {
                DeclaredFor::Declared
            } else {
                DeclaredFor::NotDeclared
            }
        }
        // The one read error that means absence, not failure: the statement is
        // about the copy the user keeps off this disk, so it stands.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DeclaredFor::Declared,
        // Everything else is an unreadable something at the recorded path —
        // the file's identity is unknown, so the declaration cannot be said
        // to match or to miss. Reported as unknown, never as saved.
        Err(_) => DeclaredFor::Unreadable,
    }
}

/// Record that the user declared they keep a copy of `path` for `scope`.
///
/// Crash-safe: temp file + fsync + atomic rename (same shape as `runstate`).
/// Merges with what is already recorded, so declaring one destination does not
/// drop another's record. A failure to persist means the scope is *not*
/// recorded — the caller must treat it as "not declared", never claim the run
/// succeeded at declaring.
///
/// A file that cannot be read is an error rather than a record without a
/// fingerprint: the fingerprint is what makes the record a statement about
/// *this* key, and a record that matched any file at that path would be the
/// false "saved" this module exists to prevent. The caller renders the error
/// and keeps the step owed.
pub fn mark_declared(state_dir: &Path, scope: &str, path: &Path) -> anyhow::Result<()> {
    let fingerprint = fingerprint_of(path).ok_or_else(|| {
        anyhow::anyhow!("cannot read key file {} to fingerprint it", path.display())
    })?;
    let mut declarations = load(state_dir);
    declarations.insert(
        scope.to_string(),
        KeyDeclaration {
            path: path.to_path_buf(),
            declared_at_unix: now_unix(),
            fingerprint,
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

    /// A key file with opaque bytes — never read as a key, only fingerprinted.
    fn plant_key(at: &Path) -> PathBuf {
        fs::create_dir_all(at.parent().expect("a parent directory")).expect("key directory");
        fs::write(at, b"opaque fixture bytes, never read as a key\n").expect("plant the key file");
        at.to_path_buf()
    }

    #[test]
    fn missing_file_reads_as_empty_not_declared() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn mark_declared_round_trips_and_is_unverified() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "backup", &key).expect("persist");
        let decls = load(dir.path());
        let entry = decls.get("backup").expect("the scope is recorded");
        assert_eq!(entry.path, key);
        assert!(!entry.declaration_is_verified, "never verified");
        assert_eq!(declared_for(&decls, "backup", &key), DeclaredFor::Declared);
    }

    /// A key file that cannot be read cannot be declared: the fingerprint is
    /// what makes the record a statement about *this* file, and a record
    /// without one would match any key later placed at that path.
    #[test]
    fn declaring_a_key_file_that_cannot_be_read_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("masterkey-backup.json");
        assert!(
            mark_declared(dir.path(), "backup", &missing).is_err(),
            "an unreadable key file has no fingerprint, so it cannot be declared"
        );
        assert!(
            load(dir.path()).is_empty(),
            "a refused declaration must record nothing"
        );
    }

    /// The whole point of the fingerprint: the same path holding a *different*
    /// key is a different file, and the declaration about the old one says
    /// nothing about it. Re-creating the key at that path is what happens after
    /// the user loses it — the moment a false "saved" costs the most.
    #[test]
    fn a_declaration_does_not_cover_a_key_re_created_at_the_same_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "backup", &key).expect("persist");
        fs::write(&key, b"a different key, created after the copy was made\n").expect("replace it");
        assert_eq!(
            declared_for(&load(dir.path()), "backup", &key),
            DeclaredFor::NotDeclared,
            "the bytes changed, so the declaration is about a key that is no longer here"
        );
    }

    /// The read-failure half of the same rule: a key file that exists but
    /// cannot be read — a directory where the key was, a file this process has
    /// no permission to open — is *not* the confirmed file the declaration
    /// describes, and must not be reported as declared-saved. Before the
    /// W284 review this case read as `true`, because every read error was
    /// folded into the "file was deleted" branch.
    #[test]
    fn a_key_replaced_by_a_directory_is_unreadable_not_declared_saved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "backup", &key).expect("persist");
        fs::remove_file(&key).expect("remove the key file");
        fs::create_dir(&key).expect("something unreadable now sits at that path");
        let state = declared_for(&load(dir.path()), "backup", &key);
        assert_eq!(
            state,
            DeclaredFor::Unreadable,
            "a file that cannot be read cannot be said to be the declared one"
        );
        assert!(
            !state.is_declared(),
            "the one-bit readers (the wizard's owed step) must see it as not declared"
        );
    }

    /// `NotFound` is the one read error that means absence rather than failure,
    /// and absence is what keeps the declaration standing: it was made about
    /// the copy the user keeps, not about this disk. The unreadable case —
    /// a file that exists but cannot be read — is the test above, and is the
    /// other half of the same split.
    #[test]
    fn a_declaration_survives_the_local_key_file_being_deleted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "backup", &key).expect("persist");
        fs::remove_file(&key).expect("remove the local copy");
        assert_eq!(
            declared_for(&load(dir.path()), "backup", &key),
            DeclaredFor::Declared,
            "the declaration is about the copy the user keeps, not about this disk"
        );
    }

    /// And a declaration is about a path: another file's declaration must not
    /// answer for this one, even in the same scope.
    #[test]
    fn a_declaration_for_another_path_does_not_cover_this_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let older = plant_key(&dir.path().join("masterkey-backup-old.json"));
        let current = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "backup", &older).expect("persist");
        assert_eq!(
            declared_for(&load(dir.path()), "backup", &current),
            DeclaredFor::NotDeclared,
            "the declaration was made about another file"
        );
    }

    /// A destination a user calls `local` must not be recorded as the local
    /// repository's key. The two are different copies with different files, and
    /// reading one's declaration as the other's would report a backup for a key
    /// nobody confirmed.
    #[test]
    fn a_destination_named_local_is_not_the_local_scope() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = plant_key(&dir.path().join("masterkey-local.json"));
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
        let local = plant_key(&dir.path().join("masterkey.json"));
        let backup = plant_key(&dir.path().join("masterkey-backup.json"));
        mark_declared(dir.path(), "local", &local).expect("a");
        mark_declared(dir.path(), "backup", &backup).expect("b");
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
                        fingerprint: "0".repeat(64),
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

    /// The version before the fingerprint read as absent, rather than as a
    /// declaration about whatever now sits at that path.
    #[test]
    fn a_version_1_record_is_not_a_declaration() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = plant_key(&dir.path().join("masterkey.json"));
        let path = key_declarations_path(dir.path());
        fs::create_dir_all(dir.path()).expect("state dir");
        let legacy = serde_json::json!({
            "version": 1,
            "declarations": {
                "local": {
                    "path": key.display().to_string(),
                    "declared_at_unix": 0,
                    "declaration_is_verified": false,
                }
            }
        });
        fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("serialise"),
        )
        .expect("write");
        assert!(
            load(dir.path()).is_empty(),
            "a record without a fingerprint cannot say which key it was about"
        );
    }
}
