//! Keyless producer: only a public inbox recipient and a posting signing key.
//! No configuration, archive key, or machine identity is resolved here.
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
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| anyhow::anyhow!("send input unreadable"))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("send input unreadable"))?;
    anyhow::ensure!(bytes.len() <= max_bytes, "send input size limit");
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("send input file name invalid"))?;
    let bundle = serde_json::json!({
        "schema": "chat-stasher/inbox@3", "kind": "harness-file",
        "harness": "unknown", "nativeSessionId": native_session_id,
        "capturedAt": chrono::Utc::now().to_rfc3339(),
        "file": { "role": "transcript", "relPath": name, "byteStart": 0,
            "byteEnd": bytes.len(), "sha256": sha256_hex(&bytes) },
        "raw": { "encoding": "base64", "data": STANDARD.encode(&bytes) },
        "fidelity": { "value": "unknown", "reason": "Explicit file without a verified platform recipe" },
        "producer": { "kind": "send", "version": env!("CARGO_PKG_VERSION"), "platform": platform, "sendKeyId": key_id }
    });
    send_bundle(
        inbox,
        &serde_json::to_vec(&bundle)?,
        key_id,
        recipient,
        signing,
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
