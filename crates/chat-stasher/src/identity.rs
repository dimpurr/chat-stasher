//! Machine identity: the stable, machine-agnostic key for a chat-stasher node.
//!
//! chat-stasher used to conflate "machine identity" with "display name" — it
//! keyed the archive partition on the *hostname* (`meta/<machine>/…`), and one
//! machine with the generic hostname `Mac` silently absorbed every other Mac
//! that shipped with the same default hostname. This module replaces that with
//! a model that keeps the two apart:
//!
//!   * **Path key** = a 128-bit random ID generated once at init and persisted
//!     locally. It is **never** derived from the hostname or any hardware
//!     identifier. (Hardware UUIDs were explicitly rejected: disk clones and
//!     Migration Assistant copies produce the *same* UUID, which would give two
//!     writers one identity — the exact class of bug this module exists to kill.)
//!   * **Display name** = mutable, repeatable, declared in the archive's
//!     declaration file; renaming it never touches old snapshots.
//!   * We **never auto-claim** an identity.
//!
//! This is a spike module: everything here is pure and testable, the path
//! helpers only *construct* paths (nothing is written to a real stage), and the
//! CLI wiring is deliberately left to the caller.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// The identity value
// ---------------------------------------------------------------------------

/// A 128-bit random machine identity.
///
/// Serializes as 32 lowercase hex characters. This is the path partition key,
/// and it is the only thing that ever *is* the machine's identity — nothing
/// else on the box is consulted to form it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MachineIdentity {
    bytes: [u8; 16],
}

impl MachineIdentity {
    /// Generate a fresh 128-bit identity from the OS CSPRNG.
    pub fn random() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 16];
        read_csprng(&mut bytes)?;
        Ok(Self { bytes })
    }

    /// Parse from 32 lowercase hex characters (the on-disk form).
    pub fn from_hex(hex: &str) -> anyhow::Result<Self> {
        let mut bytes = [0u8; 16];
        let b = hex.as_bytes();
        if b.len() != 32 || !b.iter().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("machine identity must be exactly 32 hex characters, got {hex:?}");
        }
        // All bytes are validated as hex above, so the match arm is exhaustive
        // in practice; the `_ => 0` arm exists only to keep this unwrap-free.
        let nibble = |c: u8| -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => 0,
            }
        };
        for (i, chunk) in b.chunks_exact(2).enumerate() {
            bytes[i] = (nibble(chunk[0]) << 4) | nibble(chunk[1]);
        }
        Ok(Self { bytes })
    }

    /// The canonical 32-char lowercase hex form (used as the path segment).
    pub fn as_hex(&self) -> String {
        self.bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// First 8 hex chars — enough to tell two machines apart at a glance while
    /// never disclosing the full identity. Display names are allowed to repeat,
    /// so this short id is always shown alongside them.
    pub fn short_hex(&self) -> String {
        self.as_hex()[..8].to_string()
    }
}

/// Fill `buf` from the platform CSPRNG.
#[cfg(unix)]
fn read_csprng(buf: &mut [u8]) -> anyhow::Result<()> {
    let mut f = std::fs::File::open("/dev/urandom")
        .with_context(|| "open /dev/urandom for a machine identity")?;
    f.read_exact(buf)
        .with_context(|| "read random bytes for a machine identity")?;
    Ok(())
}

/// Fallback for platforms with no OS CSPRNG we can open directly. Weaker than
/// `/dev/urandom` (clock + process counter hashed), but this branch never
/// compiles on unix, so it only exists so the module builds elsewhere rather
/// than pretending to be a strong source. See the report's "did not do".
#[cfg(not(unix))]
fn read_csprng(buf: &mut [u8]) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        // reason: only a clock before the epoch lands here; the time component is
        // then meaningless, so 0 is as honest as any value, while COUNTER still
        // varies so the hash input stays distinct within this process.
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!("{nanos}:{n}").as_bytes());
    buf.copy_from_slice(&digest[..buf.len()]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading the identity file: the three-way outcome
// ---------------------------------------------------------------------------

/// The three semantic outcomes of opening the identity path.
///
/// Deliberately **not** folded into `anyhow::Result`: adding context to a plain
/// error makes it unsafe for the init caller to tell `NotFound` apart from an
/// existing file that could not be read or parsed. `Missing` is the only
/// outcome that permits creating a new identity; `Unusable` keeps read and
/// parse failures separate so the user gets the right repair instruction.
///
/// 🔴 `Missing` and `Unusable` must never be merged. A missing identity may be
/// generated; an unreadable/unparseable one must hard-fail and tell the user
/// **not to delete the file** (see [`load_or_create`]) — this repo once lost
/// everything by replacing an unreadable key file with a fresh one.
#[derive(Debug)]
pub enum IdentityFileState {
    /// The identity path does not exist (`io::ErrorKind::NotFound`).
    Missing,
    /// The identity was read and parsed successfully.
    Loaded(MachineIdentity),
    /// The path exists but cannot be used; the inner class is actionable.
    Unusable(IdentityFileError),
}

#[derive(Debug)]
pub enum IdentityFileError {
    Read(anyhow::Error),
    Parse(anyhow::Error),
}

/// Read and classify the identity file without losing the filesystem error
/// kind. `Missing` is produced **only** by `io::ErrorKind::NotFound`.
pub fn load_identity_state(path: &Path) -> IdentityFileState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return IdentityFileState::Missing;
        }
        Err(error) => {
            return IdentityFileState::Unusable(IdentityFileError::Read(
                anyhow::Error::new(error)
                    .context(format!("cannot read identity file {}", path.display())),
            ));
        }
    };
    match MachineIdentity::from_hex(raw.trim()) {
        Ok(id) => IdentityFileState::Loaded(id),
        Err(error) => IdentityFileState::Unusable(IdentityFileError::Parse(
            error.context(format!("cannot parse identity file {}", path.display())),
        )),
    }
}

// ---------------------------------------------------------------------------
// Writing the identity file
// ---------------------------------------------------------------------------

/// Persist `id` to `path`, owner-readable only, and **do not return `Ok` until
/// it is on the disk**.
///
/// The mode is set *when the file is created*, not afterwards: a `write` then
/// `set_permissions` pair leaves a window in which the identity file is
/// world-readable, and that window is exactly what an unprivileged process on a
/// shared machine would wait for. On platforms without unix modes the file
/// inherits whatever the filesystem gives it.
///
/// Durability is part of the contract: the identity is what re-opens this
/// machine's partition on every future run, so it goes down the same route as a
/// sealed shard — temp file -> fsync -> rename, plus an fsync of the directory
/// so the rename itself is durable.
pub fn persist_identity(path: &Path, id: &MachineIdentity) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = path
        .file_name()
        .with_context(|| format!("identity path has no file name: {}", path.display()))?
        .to_owned();
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create identity directory {}", parent.display()))?;
    // A 0700 parent keeps the identity out of reach even if a later write
    // forgets its own mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort directory hardening; the file write below remains fallible"
        )]
        let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700));
    }

    let body = id.as_hex();
    let tmp = parent.join(format!(".{}.tmp", name.to_string_lossy()));

    {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // 🔴 Mode set at creation, never via a later chmod — see the doc
        // comment on this function for the window that would open.
        #[cfg(unix)]
        options.mode(0o600);
        let mut f = options
            .open(&tmp)
            .with_context(|| format!("create identity file {}", tmp.display()))?;
        use std::io::Write;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write identity file {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync identity file {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("move identity into place {}", path.display()))?;

    #[cfg(unix)]
    std::fs::File::open(&parent)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("fsync identity directory {}", parent.display()))?;

    Ok(())
}

/// Load a persisted identity, or create+persist a fresh one **only when the
/// file is missing**.
///
/// Mirrors `main.rs::masterkey()`: a `Missing` identity may be generated, but
/// an existing-but-unusable one is a hard error with the "do not delete this
/// file" instruction. The `bool` is "this identity is new". A fresh identity
/// that could not be written is an error, never a warning — it is the key to
/// this machine's partition on every later run.
pub fn load_or_create(path: &Path) -> anyhow::Result<(MachineIdentity, bool)> {
    match load_identity_state(path) {
        IdentityFileState::Loaded(id) => return Ok((id, false)),
        IdentityFileState::Missing => {}
        IdentityFileState::Unusable(IdentityFileError::Read(error)) => {
            anyhow::bail!(
                "cannot read identity file {}: {error:#}; do not delete this file; fix the access or I/O problem and re-run",
                path.display()
            );
        }
        IdentityFileState::Unusable(IdentityFileError::Parse(error)) => {
            anyhow::bail!(
                "cannot parse identity file {}: {error:#}; do not delete this file; restore a valid copy before re-running",
                path.display()
            );
        }
    }
    let id = MachineIdentity::random()?;
    persist_identity(path, &id)?;
    // Read it back rather than trusting the write: "persisted" is a claim about
    // what is on the disk, and being wrong here silently re-keys the partition.
    let written = load_identity_state(path);
    match written {
        IdentityFileState::Loaded(w) if w == id => Ok((id, true)),
        other => anyhow::bail!(
            "new identity {} does not read back as written: {other:?}",
            path.display()
        ),
    }
}

// ---------------------------------------------------------------------------
// Declaration file (display metadata)
// ---------------------------------------------------------------------------

/// The machine's self-declared display metadata, written into the archive as
/// `<stage>/meta/<machine_id>/machine.json`.
///
/// The display name lives here so it can be edited without touching any old
/// snapshot; it is allowed to repeat across machines, which is why callers must
/// always show the short id alongside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineDeclaration {
    pub machine_id: String,
    pub display_name: String,
    pub os: String,
    pub first_seen_unix: i64,
    pub declared_harnesses: Vec<String>,
}

/// Construct the declaration path: `<stage>/meta/<machine_id>/machine.json`.
pub fn machine_decl_path(stage: &Path, machine_id: &str) -> PathBuf {
    stage.join("meta").join(machine_id).join("machine.json")
}

/// Serialize a declaration to pretty JSON (this spike only builds the path and
/// the bytes; the caller decides where to write them).
pub fn serialize_declaration(decl: &MachineDeclaration) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(decl)?)
}

// ---------------------------------------------------------------------------
// Label file (display-name override, written by another machine)
// ---------------------------------------------------------------------------

/// A display-name label one machine writes *about* another, at
/// `<stage>/meta/<target_id>/label-by-<writer_id>.json`.
///
/// Several writers may label the same target; the one with the latest
/// `written_at_unix` wins ([`effective_label`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRecord {
    pub target_machine_id: String,
    pub label: String,
    pub written_by_machine_id: String,
    pub written_at_unix: i64,
}

/// Construct a label path: `<stage>/meta/<target_id>/label-by-<writer_id>.json`.
pub fn label_path(stage: &Path, target_id: &str, writer_id: &str) -> PathBuf {
    stage
        .join("meta")
        .join(target_id)
        .join(format!("label-by-{writer_id}.json"))
}

/// The label that is currently in force, together with *who wrote it*.
///
/// Latest `written_at_unix` wins. An empty set is `None` — a label is never
/// invented. The returned writer is the `written_by_machine_id` of the winning
/// record, so a human can tell which machine's opinion is currently showing.
#[derive(Debug)]
pub struct EffectiveLabel<'a> {
    pub label: &'a str,
    pub label_writer: &'a str,
    pub written_at_unix: i64,
}

pub fn effective_label(labels: &[LabelRecord]) -> Option<EffectiveLabel<'_>> {
    labels
        .iter()
        .max_by_key(|l| l.written_at_unix)
        .map(|l| EffectiveLabel {
            label: &l.label,
            label_writer: &l.written_by_machine_id,
            written_at_unix: l.written_at_unix,
        })
}

// ---------------------------------------------------------------------------
// Display name resolution
// ---------------------------------------------------------------------------

/// Marker appended when a machine has neither a label nor a declared display
/// name. The requirement is to say so explicitly — never to invent a name and
/// never to return an empty string.
const UNNAMED_MARKER: &str = "unnamed";

/// Resolve the human-facing display string for a machine.
///
/// Rules: a label wins, else the declaration's `display_name`, else the short
/// id with an explicit `unnamed` marker. The short id is **always** shown
/// (display names are allowed to repeat, so the short id is what keeps two
/// machines distinguishable). Output looks like `MacBook Air (7f3a91c2)`.
pub fn display_for(
    id: &MachineIdentity,
    decl: Option<&MachineDeclaration>,
    labels: &[LabelRecord],
) -> String {
    let short = id.short_hex();
    let label = labels
        .iter()
        .filter(|l| l.target_machine_id == id.as_hex())
        .max_by_key(|l| l.written_at_unix);
    if let Some(l) = label {
        return format!("{} ({short})", l.label);
    }
    if let Some(d) = decl.filter(|d| !d.display_name.is_empty()) {
        return format!("{} ({short})", d.display_name);
    }
    format!("{short} ({UNNAMED_MARKER})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_str(path: &Path, s: &str) {
        std::fs::write(path, s).unwrap();
    }

    fn identity(hex: &str) -> MachineIdentity {
        MachineIdentity::from_hex(hex).expect("valid hex")
    }

    fn decl(machine_id: &str, display_name: &str) -> MachineDeclaration {
        MachineDeclaration {
            machine_id: machine_id.to_string(),
            display_name: display_name.to_string(),
            os: "macos".into(),
            first_seen_unix: 0,
            declared_harnesses: vec![],
        }
    }

    // ---- 三态读取 -------------------------------------------------------

    #[test]
    fn missing_state_only_from_notfound() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = load_identity_state(&dir.path().join("no-such-id"));
        assert!(
            matches!(state, IdentityFileState::Missing),
            "an absent path must be Missing, not Unusable"
        );
    }

    #[test]
    fn loaded_state_parses_valid_hex() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        write_str(&path, "0123456789abcdef0123456789abcdef");
        match load_identity_state(&path) {
            IdentityFileState::Loaded(id) => {
                assert_eq!(id.as_hex(), "0123456789abcdef0123456789abcdef");
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn unusable_read_when_path_is_a_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = load_identity_state(dir.path());
        assert!(
            matches!(
                state,
                IdentityFileState::Unusable(IdentityFileError::Read(_))
            ),
            "reading a directory is an I/O error, not Missing"
        );
    }

    #[test]
    fn unusable_parse_on_garbage_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        write_str(&path, "not-hex-at-all");
        assert!(
            matches!(
                load_identity_state(&path),
                IdentityFileState::Unusable(IdentityFileError::Parse(_))
            ),
            "unparseable bytes must be Unusable(Parse)"
        );
    }

    #[test]
    fn missing_never_merges_with_unusable() {
        let dir = tempfile::TempDir::new().unwrap();
        // Absent path -> Missing (the only lawful producer of Missing).
        let absent = load_identity_state(&dir.path().join("nope"));
        assert!(matches!(absent, IdentityFileState::Missing));
        // Existing-but-unusable path -> Unusable, explicitly NOT Missing.
        let unusable = load_identity_state(dir.path());
        assert!(
            matches!(unusable, IdentityFileState::Unusable(_)),
            "an existing unreadable path must never come back as Missing"
        );
    }

    // ---- 生成 -----------------------------------------------------------

    #[test]
    fn random_ids_differ() {
        let a = MachineIdentity::random().unwrap();
        let b = MachineIdentity::random().unwrap();
        assert_ne!(a, b, "two generations must not collide");
    }

    #[test]
    fn identity_hex_is_32_lowercase() {
        let id = MachineIdentity::random().unwrap();
        let hex = id.as_hex();
        assert_eq!(hex.len(), 32);
        // Lowercase hex: a hexdigit that is not an uppercase letter. (Plain
        // `is_ascii_lowercase()` would wrongly reject the digits 0-9.)
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(id.short_hex(), &hex[..8]);
    }

    #[test]
    fn persist_creates_0600_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        let id = MachineIdentity::random().unwrap();
        persist_identity(&path, &id).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "identity file must be owner-only");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), id.as_hex());
    }

    #[test]
    fn load_or_create_creates_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        let (id, is_new) = load_or_create(&path).unwrap();
        assert!(is_new);
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), id.as_hex());
    }

    #[test]
    fn load_or_create_reuses_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        write_str(&path, "feedfacecafebeef0123456789abcdef");
        let (id, is_new) = load_or_create(&path).unwrap();
        assert!(!is_new);
        assert_eq!(id.as_hex(), "feedfacecafebeef0123456789abcdef");
    }

    #[test]
    fn load_or_create_refuses_unusable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("id");
        write_str(&path, "garbage-not-an-identity");
        let err = load_or_create(&path).unwrap_err().to_string();
        assert!(
            err.contains("do not delete"),
            "an unusable identity must hard-fail and tell the user not to delete it: {err}"
        );
    }

    // ---- 路径与序列化 ---------------------------------------------------

    #[test]
    fn declaration_path_shape() {
        assert_eq!(
            machine_decl_path(Path::new("/stage"), "0123456789abcdef0123456789abcdef"),
            Path::new("/stage/meta/0123456789abcdef0123456789abcdef/machine.json")
        );
    }

    #[test]
    fn label_path_shape() {
        assert_eq!(
            label_path(Path::new("/stage"), "targetid", "writerid"),
            Path::new("/stage/meta/targetid/label-by-writerid.json")
        );
    }

    #[test]
    fn serialize_declaration_roundtrips() {
        let d = decl("0123456789abcdef0123456789abcdef", "MacBook Air");
        let json = serialize_declaration(&d).unwrap();
        let back: MachineDeclaration = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert!(json.contains("display_name"));
    }

    // ---- 标签生效规则 ---------------------------------------------------

    #[test]
    fn effective_label_picks_latest_and_reports_writer() {
        let labels = vec![
            LabelRecord {
                target_machine_id: "t".into(),
                label: "old".into(),
                written_by_machine_id: "writer-a".into(),
                written_at_unix: 100,
            },
            LabelRecord {
                target_machine_id: "t".into(),
                label: "new".into(),
                written_by_machine_id: "writer-b".into(),
                written_at_unix: 200,
            },
        ];
        let eff = effective_label(&labels).expect("non-empty set");
        assert_eq!(eff.label, "new");
        assert_eq!(eff.label_writer, "writer-b");
    }

    #[test]
    fn effective_label_empty_is_none() {
        assert!(
            effective_label(&[]).is_none(),
            "empty set must be None, never invented"
        );
    }

    // ---- 显示名 ---------------------------------------------------------

    #[test]
    fn display_label_wins_over_declaration() {
        let id = identity("0123456789abcdef0123456789abcdef");
        let d = decl(&id.as_hex(), "Declared Name");
        let labels = vec![LabelRecord {
            target_machine_id: id.as_hex(),
            label: "Labelled".into(),
            written_by_machine_id: "w".into(),
            written_at_unix: 1,
        }];
        assert_eq!(display_for(&id, Some(&d), &labels), "Labelled (01234567)");
    }

    #[test]
    fn display_uses_declaration_when_no_label() {
        let id = identity("0123456789abcdef0123456789abcdef");
        let d = decl(&id.as_hex(), "MacBook Air");
        assert_eq!(display_for(&id, Some(&d), &[]), "MacBook Air (01234567)");
    }

    #[test]
    fn display_fallback_marks_unnamed_never_empty() {
        let id = identity("0123456789abcdef0123456789abcdef");
        let out = display_for(&id, None, &[]);
        assert!(!out.is_empty(), "must never return an empty string");
        assert!(
            out.contains("01234567"),
            "the short id must always be present: {out}"
        );
        assert!(
            out.contains("unnamed"),
            "a missing name must be marked explicitly, not invented: {out}"
        );
    }

    #[test]
    fn same_display_name_different_ids_are_distinguishable() {
        let id_a = identity("0123456789abcdef0123456789abcdef");
        let id_b = identity("fedcba9876543210fedcba9876543210");
        let out_a = display_for(&id_a, Some(&decl(&id_a.as_hex(), "MacBook Air")), &[]);
        let out_b = display_for(&id_b, Some(&decl(&id_b.as_hex(), "MacBook Air")), &[]);
        assert_ne!(
            out_a, out_b,
            "two machines with the same display name must stay distinguishable"
        );
        assert_eq!(out_a, "MacBook Air (01234567)");
        assert_eq!(out_b, "MacBook Air (fedcba98)");
    }
}
