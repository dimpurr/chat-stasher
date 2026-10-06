//! Opt-in remote inbox configuration. One durable record holds the locator and
//! the matching age identity/recipient, so a partial initialization cannot
//! publish a recipient whose decryption identity was not saved. This record is
//! private to the puller; producers receive only the recipient via send keys.
use age::secrecy::ExposeSecret;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

const SCHEMA: &str = "chat-stasher/remote-inbox-config@1";
const MAX_RECORD_BYTES: u64 = 65536;

/// Loaded only on the trusted archive machine. Deliberately has no Debug or
/// Serialize implementation: neither logs nor a producer can dump this value.
pub struct InboxConfig {
    pub locator: String,
    pub identity: age::x25519::Identity,
}
impl InboxConfig {
    pub fn recipient(&self) -> age::x25519::Recipient {
        self.identity.to_public()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    locator: String,
    identity: String,
    recipient: String,
}

/// Locators contain no credentials. Backend opening and storage credential
/// management are separate from initialization; this does not probe a backend.
pub fn validate(name: &str, locator: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid inbox name: use letters, digits, hyphens or underscores"
    );
    let (scheme, address) = locator
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("invalid inbox locator"))?;
    anyhow::ensure!(
        matches!(scheme, "fs" | "memory" | "s3" | "sftp" | "webdav" | "https")
            && !address.is_empty()
            && locator.len() <= 4096
            && !locator.chars().any(char::is_whitespace)
            && !locator.chars().any(char::is_control)
            && !locator.contains(['@', '?', '#']),
        "invalid inbox locator: supply a credential-free backend locator"
    );
    Ok(())
}
fn path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.json"))
}
/// Default location is alongside config.toml, without modifying that file.
pub fn default_root() -> PathBuf {
    crate::config::config_path().with_file_name("inboxes")
}

/// Absence is explicit. Unreadable, unsafe or malformed records are errors,
/// never a request to generate a replacement identity.
pub fn load(root: &Path, name: &str) -> anyhow::Result<Option<InboxConfig>> {
    validate(name, "memory://validation")?;
    let directory = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inbox configuration directory unavailable"),
    };
    anyhow::ensure!(
        directory.is_dir() && !directory.file_type().is_symlink(),
        "unsafe inbox configuration directory"
    );
    let path = path(root, name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inbox configuration read unavailable"),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "unsafe inbox configuration file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "inbox configuration must be owner-only"
        );
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .context("inbox configuration read unavailable")?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("inbox configuration read incomplete")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "inbox configuration size limit"
    );
    let record: Record = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("invalid inbox configuration; preserve the existing identity")
    })?;
    anyhow::ensure!(
        record.schema == SCHEMA,
        "unsupported inbox configuration version"
    );
    validate(name, &record.locator)?;
    let identity: age::x25519::Identity = record
        .identity
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid inbox identity; preserve the existing record"))?;
    anyhow::ensure!(
        identity.to_public().to_string() == record.recipient,
        "inbox recipient does not match its identity"
    );
    Ok(Some(InboxConfig {
        locator: record.locator,
        identity,
    }))
}

/// Initialize once, or load the same declaration. Concurrent initializers use
/// no-clobber publication: the losing process loads the winner's identity.
/// Returns true only when this call published the new record. Unix files are
/// created with mode 0600; other platforms inherit the config directory ACL.
pub fn initialize(root: &Path, name: &str, locator: &str) -> anyhow::Result<(InboxConfig, bool)> {
    validate(name, locator)?;
    if let Some(config) = load(root, name)? {
        anyhow::ensure!(
            config.locator == locator,
            "inbox already initialized with a different locator"
        );
        return Ok((config, false));
    }
    crate::test_identity_guard::refuse_fixture_write(&[name], root)?;
    std::fs::create_dir_all(root).context("inbox configuration directory unavailable")?;
    let metadata =
        std::fs::symlink_metadata(root).context("inbox configuration directory unavailable")?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe inbox configuration directory"
    );
    let identity = age::x25519::Identity::generate();
    let record = Record {
        schema: SCHEMA.into(),
        locator: locator.into(),
        identity: identity.to_string().expose_secret().to_owned(),
        recipient: identity.to_public().to_string(),
    };
    let mut temporary =
        tempfile::NamedTempFile::new_in(root).context("inbox configuration write unavailable")?;
    // NamedTempFile creates Unix files with mode 0600 before writing any bytes.
    temporary
        .write_all(&serde_json::to_vec(&record)?)
        .context("inbox configuration write incomplete")?;
    temporary
        .as_file()
        .sync_all()
        .context("inbox configuration sync unavailable")?;
    let created = match temporary.persist_noclobber(path(root, name)) {
        Ok(_) => true,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error.error).context("inbox configuration publication unavailable")
        }
    };
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|dir| dir.sync_all())
        .context("inbox configuration directory sync unavailable")?;
    let config = load(root, name)?
        .ok_or_else(|| anyhow::anyhow!("inbox configuration disappeared after initialization"))?;
    anyhow::ensure!(
        config.locator == locator,
        "inbox already initialized with a different locator"
    );
    Ok((config, created))
}
