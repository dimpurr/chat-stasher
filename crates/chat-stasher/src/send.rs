//! Keyless producer: only a public inbox recipient and a posting signing key.
//! No archive configuration, archive key, or machine identity is resolved here.
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;

pub(crate) const ENVELOPE_SCHEMA: &str = "chat-stasher/remote-inbox@1";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub schema: String,
    pub key_id: String,
    pub ciphertext: String,
    pub signature: String,
}
/// Domain separation binds the envelope version and key id to the ciphertext.
pub(crate) fn signed_bytes(key_id: &str, ciphertext: &[u8]) -> Vec<u8> {
    let mut bytes = ENVELOPE_SCHEMA.as_bytes().to_vec();
    bytes.extend_from_slice(&(key_id.len() as u64).to_be_bytes());
    bytes.extend_from_slice(key_id.as_bytes());
    bytes.extend_from_slice(ciphertext);
    bytes
}

/// Encrypt a validated capture and upload under an opaque, ciphertext-derived key.
/// Retrying may produce another object; content deduplication belongs to the sink.
pub fn send_bundle(
    inbox: &crate::remote_inbox::RemoteInbox,
    bundle: &[u8],
    key_id: &str,
    recipient: &age::x25519::Recipient,
    signing: &SigningKey,
) -> anyhow::Result<String> {
    crate::inbox::check_bundle(bundle).map_err(|_| anyhow::anyhow!("invalid send bundle"))?;
    let value: serde_json::Value = serde_json::from_slice(bundle)?;
    anyhow::ensure!(
        value["schema"] == "chat-stasher/inbox@3" && value["kind"] == "harness-file",
        "send requires a v3 harness-file capture"
    );
    anyhow::ensure!(
        value["producer"]["kind"] == "send" && value["producer"]["sendKeyId"] == key_id,
        "send key provenance mismatch"
    );
    let mut ciphertext = Vec::new();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(recipient as &dyn age::Recipient))?;
    let mut writer = encryptor.wrap_output(&mut ciphertext)?;
    writer.write_all(bundle)?;
    writer.finish()?;
    let signature = signing.sign(&signed_bytes(key_id, &ciphertext));
    let object = sha256_hex(&ciphertext);
    let envelope = Envelope {
        schema: ENVELOPE_SCHEMA.into(),
        key_id: key_id.into(),
        ciphertext: STANDARD.encode(ciphertext),
        signature: STANDARD.encode(signature.to_bytes()),
    };
    inbox.upload(&object, serde_json::to_vec(&envelope)?)?;
    Ok(object)
}

/// Explicit-file producer for platforms without a verified recipe. The caller
/// supplies the native session id; no machine identity is created. Absolute
/// paths stay on the producer and only the file name enters the capture.
pub fn send_path(
    inbox: &crate::remote_inbox::RemoteInbox,
    path: &std::path::Path,
    native_session_id: &str,
    platform: &str,
    key_id: &str,
    recipient: &age::x25519::Recipient,
    signing: &SigningKey,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let bundle = explicit_bundle(path, native_session_id, platform, key_id, max_bytes, None)?;
    send_bundle(inbox, &bundle, key_id, recipient, signing)
}

/// Observed account axis for an explicitly selected registry harness. The
/// account is a stable platform UID, never a machine, key label or partition.
pub struct CaptureIdentity {
    pub harness: String,
    pub account: Option<String>,
}

fn explicit_bundle(
    path: &std::path::Path,
    native_session_id: &str,
    platform: &str,
    key_id: &str,
    max_bytes: usize,
    identity: Option<&CaptureIdentity>,
) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "send input must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= max_bytes, "send input size limit");
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("send input file name invalid"))?;
    let mut bundle = serde_json::json!({
        "schema": "chat-stasher/inbox@3", "kind": "harness-file",
        "harness": identity.map_or("unknown", |capture| capture.harness.as_str()), "nativeSessionId": native_session_id,
        "capturedAt": chrono::Utc::now().to_rfc3339(),
        "file": { "role": "transcript", "relPath": name, "byteStart": 0,
            "byteEnd": bytes.len(), "sha256": sha256_hex(&bytes) },
        "raw": { "encoding": "base64", "data": STANDARD.encode(&bytes) },
        "fidelity": { "value": "unknown", "reason": "Explicit file without a verified platform recipe" },
        "dimensions": { "surface": ["cloud"] },
        "producer": { "kind": "send", "version": env!("CARGO_PKG_VERSION"), "platform": platform, "sendKeyId": key_id }
    });
    if let Some(account) = identity.and_then(|capture| capture.account.as_deref()) {
        bundle["identity"] = serde_json::json!({"level": "platform_uid", "value": account});
    }
    let bytes = serde_json::to_vec(&bundle)?;
    crate::inbox::check_bundle(&bytes).map_err(|_| anyhow::anyhow!("invalid send capture"))?;
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bounded diagnostics: backend paths, input bytes and posting secrets cannot
/// enter this error. Incomplete reads/uploads carry exit 3; refusals carry 1.
#[derive(Debug)]
pub struct SendFailure {
    pub reason: &'static str,
    pub incomplete: bool,
}
impl SendFailure {
    fn refused(reason: &'static str) -> Self {
        Self {
            reason,
            incomplete: false,
        }
    }
    fn incomplete(reason: &'static str) -> Self {
        Self {
            reason,
            incomplete: true,
        }
    }
}
pub struct SendReport {
    pub missing_account: bool,
    pub resumed: bool,
    pub cursor_saved: bool,
}

/// Complete explicit-file producer path. Filesystem inboxes use the host's
/// permissions; embedded provider credentials are not used for this backend.
/// Other locators are deliberately refused until their option mapping is wired.
/// The cursor is only a local upload receipt, never archive proof. Losing it
/// causes a resend; a successful receipt does not say the bytes are archived.
pub fn send_explicit(
    path: &std::path::Path,
    session: &str,
    key: &crate::send_key::SendKey,
    now: u64,
    cursor_root: &std::path::Path,
) -> Result<SendReport, SendFailure> {
    send_explicit_identified(path, session, key, now, cursor_root, None)
}

/// Send a named harness capture without inferring any unobserved identity or
/// fidelity. Explicit files retain unknown fidelity even for a known harness.
/// Registry failures are incomplete reads; an unregistered harness is a refusal.
pub fn send_explicit_identified(
    path: &std::path::Path,
    session: &str,
    key: &crate::send_key::SendKey,
    now: u64,
    cursor_root: &std::path::Path,
    identity: Option<&CaptureIdentity>,
) -> Result<SendReport, SendFailure> {
    if now >= key.expires_at || now < key.issued_at {
        return Err(SendFailure::refused("posting key expired or not yet valid"));
    }
    if let Some(capture) = identity {
        let registry = crate::scanner::load_registry_from_repo()
            .map_err(|_| SendFailure::incomplete("harness registry unreadable"))?;
        if capture.harness == "unknown"
            || !registry.harnesses.iter().any(|h| h.id == capture.harness)
        {
            return Err(SendFailure::refused("harness not registered"));
        }
    }
    let missing_account = identity
        .and_then(|capture| capture.account.as_ref())
        .is_none();
    let root = key
        .locator
        .strip_prefix("fs://")
        .map(std::path::Path::new)
        .filter(|root| root.is_absolute())
        .ok_or_else(|| SendFailure::refused("only absolute fs:// inbox locators are wired"))?;
    let bundle = explicit_bundle(
        path,
        session,
        &key.platform,
        &key.key_id,
        4 * 1024 * 1024,
        identity,
    )
    .map_err(|e| {
        if e.downcast_ref::<std::io::Error>().is_some() {
            SendFailure::incomplete("input unreadable")
        } else {
            SendFailure::refused("input size or capture contract invalid")
        }
    })?;
    // Exclude capture time, but bind every other capture field and the inbox.
    // Changes to bytes, scope, key, session, filename or producer version resend.
    let mut fingerprint: serde_json::Value = serde_json::from_slice(&bundle)
        .map_err(|_| SendFailure::refused("capture contract invalid"))?;
    fingerprint
        .as_object_mut()
        .ok_or_else(|| SendFailure::refused("capture contract invalid"))?
        .remove("capturedAt");
    fingerprint["inboxLocator"] = key.locator.clone().into();
    let digest = sha256_hex(
        &serde_json::to_vec(&fingerprint)
            .map_err(|_| SendFailure::refused("capture contract invalid"))?,
    );
    let receipt = format!("chat-stasher/send-cursor@1\n{digest}\n");
    let cursor = cursor_root.join(&digest);
    if valid_receipt(&cursor, receipt.as_bytes()) {
        return Ok(SendReport {
            resumed: true,
            cursor_saved: true,
            missing_account,
        });
    }
    let mut observed_identity = vec![session, key.platform.as_str()];
    if let Some(capture) = identity {
        observed_identity.push(&capture.harness);
        if let Some(account) = capture.account.as_deref() {
            observed_identity.push(account);
        }
    }
    crate::test_identity_guard::refuse_fixture_write(&observed_identity, root)
        .map_err(|_| SendFailure::refused("fixture inbox write refused"))?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|_| SendFailure::incomplete("backend runtime unavailable"))?;
    let entered = runtime.enter();
    let staging = root.join(".chat-stasher-tmp");
    let root_text = root
        .to_str()
        .ok_or_else(|| SendFailure::refused("invalid filesystem locator"))?;
    let staging_text = staging
        .to_str()
        .ok_or_else(|| SendFailure::refused("invalid filesystem locator"))?;
    // Same-filesystem rename keeps concurrent pullers from seeing partial objects.
    let operator = opendal::Operator::new(
        opendal::services::Fs::default()
            .root(root_text)
            .atomic_write_dir(staging_text),
    )
    .map_err(|_| SendFailure::incomplete("inbox backend unavailable"))?
    .finish();
    let operator = opendal::blocking::Operator::new(operator)
        .map_err(|_| SendFailure::incomplete("inbox backend unavailable"))?;
    drop(entered);
    let inbox = crate::remote_inbox::RemoteInbox::new(operator, 10 * 1024 * 1024);
    send_bundle(&inbox, &bundle, &key.key_id, &key.recipient, &key.signing)
        .map_err(|_| SendFailure::incomplete("inbox upload unavailable"))?;
    // Only a completed upload can advance the local best-effort receipt.
    let cursor_saved = save_receipt(cursor_root, &cursor, receipt.as_bytes(), session).is_ok();
    Ok(SendReport {
        resumed: false,
        cursor_saved,
        missing_account,
    })
}
fn valid_receipt(path: &std::path::Path, expected: &[u8]) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != expected.len() as u64
    {
        return false;
    }
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes == expected
}
fn save_receipt(
    root: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
    session: &str,
) -> anyhow::Result<()> {
    crate::test_identity_guard::refuse_fixture_write(&[session], root)?;
    std::fs::create_dir_all(root)?;
    let metadata = std::fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe cursor directory"
    );
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
