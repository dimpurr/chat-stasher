//! Portable posting secrets and immutable trusted puller policies. Issuance
//! never opens a backend or loads an archive key. Revocation is an append-only
//! marker, so concurrent issuance/revocation cannot lose another key's state.
use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const KEY_SCHEMA: &str = "chat-stasher/send-key@1";
const POLICY_SCHEMA: &str = "chat-stasher/send-key-policy@1";
const REVOKE_SCHEMA: &str = "chat-stasher/send-key-revocation@1";
const PREFIX: &str = "cs-send-v1.";
const MAX_BYTES: usize = 65536;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostingRecord {
    schema: String,
    locator: String,
    credential: BTreeMap<String, String>,
    recipient: String,
    signing_secret: String,
    key_id: String,
    label: String,
    platform: String,
    issued_at: u64,
    expires_at: u64,
}
/// No Debug or Serialize: exporting the secret is an explicit issuance action.
/// The supplied credential's write scope must be established by its provider;
/// parsing this string cannot prove permissions. Pull trusts its own policy.
pub struct SendKey {
    pub locator: String,
    pub credential: BTreeMap<String, String>,
    pub recipient: age::x25519::Recipient,
    pub signing: SigningKey,
    pub key_id: String,
    pub label: String,
    pub platform: String,
    pub issued_at: u64,
    pub expires_at: u64,
}
impl SendKey {
    pub fn parse(token: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(token.len() <= MAX_BYTES * 2, "send key size limit");
        let encoded = token
            .strip_prefix(PREFIX)
            .ok_or_else(|| anyhow::anyhow!("unsupported send key version"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("invalid send key encoding"))?;
        anyhow::ensure!(bytes.len() <= MAX_BYTES, "send key size limit");
        let record: PostingRecord = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid send key record"))?;
        anyhow::ensure!(record.schema == KEY_SCHEMA, "unsupported send key version");
        crate::inbox_config::validate("validation", &record.locator)?;
        validate_metadata(&record.platform, &record.label)?;
        validate_credential(&record.credential)?;
        anyhow::ensure!(
            record.expires_at > record.issued_at,
            "invalid send key expiry"
        );
        let secret = decode_32(&record.signing_secret)?;
        let signing = SigningKey::from_bytes(&secret);
        anyhow::ensure!(
            record.key_id == key_id(&signing.verifying_key()),
            "send key signing identity mismatch"
        );
        let recipient = record
            .recipient
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid send key recipient"))?;
        Ok(Self {
            locator: record.locator,
            credential: record.credential,
            recipient,
            signing,
            key_id: record.key_id,
            label: record.label,
            platform: record.platform,
            issued_at: record.issued_at,
            expires_at: record.expires_at,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyRecord {
    schema: String,
    key_id: String,
    public_key: String,
    label: String,
    platform: String,
    issued_at: u64,
    expires_at: u64,
    max_bundle_bytes: usize,
    max_objects_per_pull: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Revocation {
    schema: String,
    key_id: String,
}
#[derive(Serialize)]
pub struct KeySummary {
    pub key_id: String,
    pub label: String,
    pub platform: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub state: &'static str,
}
/// Limits are persisted on the trusted machine, never taken from a posting key.
/// The object ceiling applies per pull and per rolling hour. Existing v1 policy
/// records retain their ceiling when loaded; no posting-key field sets a quota.
pub struct IssueOptions<'a> {
    pub platform: &'a str,
    pub label: &'a str,
    pub lifetime_secs: u64,
    pub max_bundle_bytes: usize,
    pub max_objects_per_pull: usize,
}
/// Parse a positive explicit duration in hours or days; omitted CLI expiry is
/// 24 hours. Overflow is a usage error rather than a long-lived key by accident.
pub fn duration_secs(value: &str) -> anyhow::Result<u64> {
    let (digits, factor) = if let Some(n) = value.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = value.strip_suffix('d') {
        (n, 86400)
    } else {
        anyhow::bail!("invalid send key expiry: use positive hours or days");
    };
    anyhow::ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "invalid send key expiry"
    );
    let secs = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow::anyhow!("invalid send key expiry"))?;
    Ok(secs)
}
pub fn validate_metadata(platform: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !platform.is_empty()
            && platform.len() <= 64
            && platform
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid send key platform"
    );
    anyhow::ensure!(
        !label.is_empty() && label.len() <= 128 && label.bytes().all(|b| (32..=126).contains(&b)),
        "invalid send key label"
    );
    Ok(())
}
fn validate_credential(credential: &BTreeMap<String, String>) -> anyhow::Result<()> {
    anyhow::ensure!(
        !credential.is_empty()
            && credential.len() <= 32
            && credential.iter().all(|(k, v)| !k.is_empty()
                && k.len() <= 128
                && k.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                && !v.is_empty()
                && v.len() <= 16384),
        "invalid posting credential record"
    );
    Ok(())
}
fn decode_32(value: &str) -> anyhow::Result<[u8; 32]> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid send key cryptographic material"))
}
fn key_id(public: &VerifyingKey) -> String {
    Sha256::digest(public.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn validate_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid send key id"
    );
    Ok(())
}
fn directory(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.keys"))
}
fn check_directory(path: &Path) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "unsafe send key directory"
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("send key directory unavailable"),
    }
}
fn read(path: &Path, private: bool) -> anyhow::Result<Vec<u8>> {
    let m = std::fs::symlink_metadata(path).context("send key record read unavailable")?;
    anyhow::ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "unsafe send key record"
    );
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            m.permissions().mode() & 0o077 == 0,
            "send key record must be owner-only"
        );
    }
    #[cfg(not(unix))]
    let _ = private; // Non-Unix records inherit the configuration directory ACL.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .context("send key record read unavailable")?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("send key record read incomplete")?;
    anyhow::ensure!(bytes.len() <= MAX_BYTES, "send key record size limit");
    Ok(bytes)
}
fn publish(dir: &Path, filename: &str, bytes: &[u8]) -> anyhow::Result<bool> {
    anyhow::ensure!(check_directory(dir)?, "send key directory disappeared");
    let mut temp = tempfile::NamedTempFile::new_in(dir).context("send key write unavailable")?;
    temp.write_all(bytes).context("send key write incomplete")?;
    temp.as_file()
        .sync_all()
        .context("send key sync unavailable")?;
    let created = match temp.persist_noclobber(dir.join(filename)) {
        Ok(_) => true,
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.error).context("send key publication unavailable"),
    };
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .context("send key directory sync unavailable")?;
    Ok(created)
}
fn configured(root: &Path, name: &str) -> anyhow::Result<crate::inbox_config::InboxConfig> {
    crate::inbox_config::load(root, name)?
        .ok_or_else(|| anyhow::anyhow!("inbox is not initialized"))
}
/// Return the posting string only after its public policy is durable. The
/// credential file is a bounded JSON map of provider-specific string settings.
/// It is neither printed in diagnostics nor copied into trusted policy files.
pub fn issue(
    root: &Path,
    name: &str,
    credential_file: &Path,
    options: &IssueOptions<'_>,
    now: u64,
) -> anyhow::Result<String> {
    validate_metadata(options.platform, options.label)?;
    anyhow::ensure!(
        options.lifetime_secs > 0
            && options.max_bundle_bytes > 0
            && options.max_objects_per_pull > 0,
        "invalid send key limits"
    );
    let expires_at = now
        .checked_add(options.lifetime_secs)
        .ok_or_else(|| anyhow::anyhow!("invalid send key expiry"))?;
    let config = configured(root, name)?;
    let credential: BTreeMap<String, String> =
        serde_json::from_slice(&read(credential_file, false)?)
            .map_err(|_| anyhow::anyhow!("invalid posting credential record"))?;
    validate_credential(&credential)?;
    let mut secret = [0; 32];
    crate::view::os_random(&mut secret).context("send key randomness unavailable")?;
    let signing = SigningKey::from_bytes(&secret);
    let id = key_id(&signing.verifying_key());
    let recipient = config.recipient().to_string();
    let posting = PostingRecord {
        schema: KEY_SCHEMA.into(),
        locator: config.locator,
        credential,
        recipient,
        signing_secret: URL_SAFE_NO_PAD.encode(secret),
        key_id: id.clone(),
        label: options.label.into(),
        platform: options.platform.into(),
        issued_at: now,
        expires_at,
    };
    let bytes = serde_json::to_vec(&posting)?;
    anyhow::ensure!(bytes.len() <= MAX_BYTES, "send key size limit");
    let token = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let policy = PolicyRecord {
        schema: POLICY_SCHEMA.into(),
        key_id: id.clone(),
        public_key: URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes()),
        label: options.label.into(),
        platform: options.platform.into(),
        issued_at: now,
        expires_at,
        max_bundle_bytes: options.max_bundle_bytes,
        max_objects_per_pull: options.max_objects_per_pull,
    };
    crate::test_identity_guard::refuse_fixture_write(
        &[name, options.platform, options.label],
        root,
    )?;
    let dir = directory(root, name);
    std::fs::create_dir_all(&dir).context("send key directory unavailable")?;
    anyhow::ensure!(
        publish(&dir, &format!("{id}.json"), &serde_json::to_vec(&policy)?)?,
        "send key id already exists"
    );
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|d| d.sync_all())
        .context("send key parent directory sync unavailable")?;
    Ok(token)
}
fn policy(dir: &Path, id: &str) -> anyhow::Result<(PolicyRecord, crate::remote_inbox::KeyPolicy)> {
    validate_id(id)?;
    let record: PolicyRecord =
        serde_json::from_slice(&read(&dir.join(format!("{id}.json")), true)?)
            .map_err(|_| anyhow::anyhow!("invalid send key policy"))?;
    anyhow::ensure!(
        record.schema == POLICY_SCHEMA && record.key_id == id,
        "invalid send key policy identity or version"
    );
    validate_metadata(&record.platform, &record.label)?;
    let public_key = VerifyingKey::from_bytes(&decode_32(&record.public_key)?)
        .map_err(|_| anyhow::anyhow!("invalid send key public key"))?;
    anyhow::ensure!(
        key_id(&public_key) == id
            && record.expires_at > record.issued_at
            && record.max_bundle_bytes > 0
            && record.max_objects_per_pull > 0,
        "invalid send key policy"
    );
    let revoked = match std::fs::symlink_metadata(dir.join(format!("{id}.revoked"))) {
        Ok(_) => {
            let marker: Revocation =
                serde_json::from_slice(&read(&dir.join(format!("{id}.revoked")), true)?)
                    .map_err(|_| anyhow::anyhow!("invalid send key revocation"))?;
            anyhow::ensure!(
                marker.schema == REVOKE_SCHEMA && marker.key_id == id,
                "invalid send key revocation"
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).context("send key revocation read unavailable"),
    };
    let policy = crate::remote_inbox::KeyPolicy {
        public_key,
        platform: record.platform.clone(),
        expires_at: record.expires_at,
        revoked,
        max_bundle_bytes: record.max_bundle_bytes,
        max_objects_per_pull: record.max_objects_per_pull,
    };
    Ok((record, policy))
}
/// Absence of policies for a declared inbox is a measured empty set; malformed
/// or unreadable entries fail the whole load, never silently disappear.
pub fn load_policies(
    root: &Path,
    name: &str,
    now: u64,
) -> anyhow::Result<(
    BTreeMap<String, crate::remote_inbox::KeyPolicy>,
    Vec<KeySummary>,
)> {
    configured(root, name)?;
    let dir = directory(root, name);
    let mut policies = BTreeMap::new();
    let mut summaries = Vec::new();
    if !check_directory(&dir)? {
        return Ok((policies, summaries));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut revoked_ids = Vec::new();
    for entry in std::fs::read_dir(&dir).context("send key listing unavailable")? {
        let entry = entry.context("send key listing incomplete")?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid send key record name"))?;
        if let Some(id) = name.strip_suffix(".json") {
            validate_id(id)?;
            ids.insert(id.to_owned());
        } else if let Some(id) = name.strip_suffix(".revoked") {
            validate_id(id)?;
            revoked_ids.push(id.to_owned());
        } else if name.starts_with(".tmp") {
            continue;
        }
        // NamedTempFile may be mid-publication in another process.
        else {
            anyhow::bail!("invalid send key record name");
        }
    }
    anyhow::ensure!(
        revoked_ids.iter().all(|id| ids.contains(id)),
        "orphan send key revocation"
    );
    for id in ids {
        let (record, policy) = policy(&dir, &id)?;
        let state = if policy.revoked {
            "revoked"
        } else if now >= policy.expires_at {
            "expired"
        } else {
            "active"
        };
        summaries.push(KeySummary {
            key_id: id.clone(),
            label: record.label,
            platform: record.platform,
            issued_at: record.issued_at,
            expires_at: record.expires_at,
            state,
        });
        policies.insert(id, policy);
    }
    Ok((policies, summaries))
}
/// Revocation is idempotent and irreversible here; the public policy remains
/// available to report the reason for refusing pending objects.
pub fn revoke(root: &Path, name: &str, id: &str) -> anyhow::Result<()> {
    validate_id(id)?;
    configured(root, name)?;
    let dir = directory(root, name);
    anyhow::ensure!(check_directory(&dir)?, "send key is not issued");
    match std::fs::symlink_metadata(dir.join(format!("{id}.json"))) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("send key is not issued")
        }
        Err(e) => return Err(e).context("send key policy read unavailable"),
        Ok(_) => {}
    }
    policy(&dir, id)?;
    crate::test_identity_guard::refuse_fixture_write(&[name], root)?;
    let marker = Revocation {
        schema: REVOKE_SCHEMA.into(),
        key_id: id.into(),
    };
    publish(
        &dir,
        &format!("{id}.revoked"),
        &serde_json::to_vec(&marker)?,
    )?;
    // Validate a concurrent winner rather than treating arbitrary bytes as revoked.
    anyhow::ensure!(
        policy(&dir, id)?.1.revoked,
        "send key revocation disappeared"
    );
    Ok(())
}
