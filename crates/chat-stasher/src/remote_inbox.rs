//! Optional encrypted inbox transport and puller. Core collection does not
//! depend on this module. Retirement is allowed only after the shared sink's
//! durable Stored/Duplicate result and independent archive proof. Refused
//! objects remain available for review.
use crate::bundle_transport::{BundleListing, BundleTransport, ListedBundle};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use std::{collections::BTreeMap, io::Read, path::Path};

pub struct RemoteInbox {
    operator: opendal::blocking::Operator,
    max_object_bytes: usize,
}
impl RemoteInbox {
    /// The caller owns the OpenDAL runtime and backend credentials. Use an
    /// isolated operator root dedicated to this inbox, never a destination.
    pub fn new(operator: opendal::blocking::Operator, max_object_bytes: usize) -> Self {
        Self {
            operator,
            max_object_bytes,
        }
    }
    pub(crate) fn upload(&self, object: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(opaque_key(object), "invalid inbox object key");
        anyhow::ensure!(
            bytes.len() <= self.max_object_bytes,
            "inbox object size limit"
        );
        self.operator
            .write(object, bytes)
            .map_err(|_| anyhow::anyhow!("inbox upload unavailable"))?;
        Ok(())
    }
}
fn opaque_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl BundleTransport for RemoteInbox {
    type Item = String;
    fn list(&self) -> anyhow::Result<BundleListing<String>> {
        let entries = self
            .operator
            .list("/")
            .map_err(|_| anyhow::anyhow!("inbox listing unavailable: waiting count unknown"))?;
        let mut listing = BundleListing::default();
        for entry in entries {
            if entry.metadata().is_dir() {
                continue;
            }
            listing.total_inbox_files += 1;
            if opaque_key(entry.path()) {
                listing.items.push(ListedBundle {
                    name: entry.path().into(),
                    item: entry.path().into(),
                });
            } else {
                // Never echo backend paths: untrusted names can contain secrets.
                listing
                    .errors
                    .push(("(opaque object)".into(), "invalid inbox object key".into()));
            }
        }
        listing.items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(listing)
    }
    fn fetch(&self, item: &String) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(opaque_key(item), "invalid inbox object key");
        // Pin a bounded range at fetch time. Some backends reject ranges past
        // EOF; a later grow cannot enlarge this allocation, and a shrink is
        // an incomplete fetch rather than a shorter successful object.
        let length = self
            .operator
            .stat(item)
            .map_err(|_| anyhow::anyhow!("inbox fetch unavailable"))?
            .content_length();
        anyhow::ensure!(
            length <= self.max_object_bytes as u64,
            "inbox object size limit"
        );
        let bytes = self
            .operator
            .reader(item)
            .map_err(|_| anyhow::anyhow!("inbox fetch unavailable"))?
            .read(0..length)
            .map_err(|_| anyhow::anyhow!("inbox fetch unavailable"))?
            .to_vec();
        anyhow::ensure!(bytes.len() as u64 == length, "inbox fetch incomplete");
        Ok(bytes)
    }
    fn retire(&self, name: &str, item: &String) -> anyhow::Result<()> {
        anyhow::ensure!(name == item && opaque_key(item), "invalid inbox retirement");
        self.operator
            .delete(item)
            .map_err(|_| anyhow::anyhow!("inbox retirement unavailable"))?;
        Ok(())
    }
}

/// Trusted pull-time state, supplied by the archive machine; never by a sender.
/// The object ceiling applies both per invocation and over a rolling hour.
/// The caller must retain one rate-state file per inbox across invocations.
#[derive(Clone)]
pub struct KeyPolicy {
    pub public_key: VerifyingKey,
    pub platform: String,
    /// Unix seconds; the expiry instant itself is refused.
    pub expires_at: u64,
    pub revoked: bool,
    pub max_bundle_bytes: usize,
    /// Maximum authenticated attempts per invocation and rolling hour.
    pub max_objects_per_pull: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Envelope,
    UnknownKey,
    RevokedKey,
    ExpiredKey,
    SizeLimit,
    RateLimit,
    /// Accounting unavailable or clock regression; allowance is unknown.
    RateAccounting,
    Signature,
    Decrypt,
    Contract,
    Scope,
    Seal,
    Fetch,
    Retire,
    Unproven,
}
#[derive(Debug, Default)]
pub struct PullReport {
    pub waiting: usize,
    pub stored: usize,
    pub duplicates: usize,
    pub refused: Vec<Refusal>,
}

fn open(
    bytes: &[u8],
    identity: &age::x25519::Identity,
    keys: &BTreeMap<String, KeyPolicy>,
    now: u64,
    used: &mut BTreeMap<String, usize>,
    rate_state: &Path,
) -> Result<(Vec<u8>, Vec<String>), Refusal> {
    let envelope: crate::send::Envelope =
        serde_json::from_slice(bytes).map_err(|_| Refusal::Envelope)?;
    if envelope.schema != crate::send::ENVELOPE_SCHEMA {
        return Err(Refusal::Envelope);
    }
    let policy = keys.get(&envelope.key_id).ok_or(Refusal::UnknownKey)?;
    if policy.revoked {
        return Err(Refusal::RevokedKey);
    }
    if now >= policy.expires_at {
        return Err(Refusal::ExpiredKey);
    }
    let ciphertext = STANDARD
        .decode(envelope.ciphertext)
        .map_err(|_| Refusal::Envelope)?;
    let signature = Signature::from_slice(
        &STANDARD
            .decode(envelope.signature)
            .map_err(|_| Refusal::Signature)?,
    )
    .map_err(|_| Refusal::Signature)?;
    policy
        .public_key
        .verify_strict(
            &crate::send::signed_bytes(&envelope.key_id, &ciphertext),
            &signature,
        )
        .map_err(|_| Refusal::Signature)?;
    let count = used.entry(envelope.key_id.clone()).or_default();
    if *count >= policy.max_objects_per_pull {
        return Err(Refusal::RateLimit);
    }
    if !crate::inbox_rate::reserve(
        rate_state,
        &envelope.key_id,
        now,
        policy.max_objects_per_pull,
    )
    .map_err(|_| Refusal::RateAccounting)?
    {
        return Err(Refusal::RateLimit);
    }
    *count += 1;
    let decryptor = age::Decryptor::new(&ciphertext[..]).map_err(|_| Refusal::Decrypt)?;
    let reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|_| Refusal::Decrypt)?;
    let mut bundle = Vec::new();
    reader
        .take(policy.max_bundle_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bundle)
        .map_err(|_| Refusal::Decrypt)?;
    if bundle.len() > policy.max_bundle_bytes {
        return Err(Refusal::SizeLimit);
    }
    crate::inbox::check_bundle(&bundle).map_err(|_| Refusal::Contract)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bundle).map_err(|_| Refusal::Contract)?;
    if value["schema"] != "chat-stasher/inbox@3" || value["kind"] != "harness-file" {
        return Err(Refusal::Contract);
    }
    if value["producer"]["kind"] != "send"
        || value["producer"]["sendKeyId"] != envelope.key_id
        || value["producer"]["platform"] != policy.platform
    {
        return Err(Refusal::Scope);
    }
    // Check the identity components before the caller creates a stage root.
    // This preserves the sink's fixture fail-safe at the first write boundary.
    let axes = [
        &value["harness"],
        &value["nativeSessionId"],
        &value["identity"]["value"],
    ]
    .into_iter()
    .filter_map(|v| v.as_str())
    .map(str::to_owned)
    .collect();
    Ok((bundle, axes))
}
/// Trusted archive-machine read-back, independent of the stage and any cursor.
/// Implementations must consult every declared destination and compare its
/// bytes/digest with the sealed session, as reclaim-stage does. No destinations,
/// unreadable archives and incomplete reads must never produce `true`.
pub trait ArchiveProof {
    fn holds(&self, machine: &str, outcome: &crate::inbox::SealOutcome) -> anyhow::Result<bool>;
}

/// Listing failure is an error (unknown), never a successful zero-object pass.
/// Per-object failures are bounded, named reasons, with no backend/body errors.
/// `now` is Unix seconds from the trusted puller clock. `rate_state` is a
/// trusted, owner-only SQLite file with an existing parent, separate from stage.
/// Keep the same file for this inbox on every invocation. Authenticated attempts
/// consume quota even if decryption, sealing, proof or retirement later fails.
/// Invalid signatures consume no quota; unavailable accounting fails closed.
pub fn pull<T: BundleTransport, P: ArchiveProof>(
    transport: &T,
    identity: &age::x25519::Identity,
    keys: &BTreeMap<String, KeyPolicy>,
    now: u64,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    rate_state: &Path,
    archive: &P,
) -> anyhow::Result<PullReport> {
    let listing = transport.list()?;
    let mut report = PullReport {
        waiting: listing.total_inbox_files,
        ..PullReport::default()
    };
    report
        .refused
        .extend(listing.errors.iter().map(|_| Refusal::Fetch));
    let mut used = BTreeMap::new();
    for item in listing.items {
        let result = (|| {
            let bytes = transport.fetch(&item.item).map_err(|_| Refusal::Fetch)?;
            let (bundle, axes) = open(&bytes, identity, keys, now, &mut used, rate_state)?;
            let mut identities: Vec<&str> = axes.iter().map(String::as_str).collect();
            identities.push(machine);
            crate::test_identity_guard::refuse_fixture_write(&identities, stage)
                .map_err(|_| Refusal::Seal)?;
            std::fs::create_dir_all(stage).map_err(|_| Refusal::Seal)?;
            let outcome = crate::inbox::seal_payload(
                &item.name, &bundle, stage, machine, bucket_cap, None, None,
            )
            .map_err(|_| Refusal::Seal)?;
            match &outcome {
                crate::inbox::SealOutcome::Stored(_) => report.stored += 1,
                crate::inbox::SealOutcome::Duplicate(_) => report.duplicates += 1,
            }
            if !archive
                .holds(machine, &outcome)
                .map_err(|_| Refusal::Unproven)?
            {
                return Err(Refusal::Unproven);
            }
            transport
                .retire(&item.name, &item.item)
                .map_err(|_| Refusal::Retire)
        })();
        if let Err(reason) = result {
            report.refused.push(reason);
        }
    }
    Ok(report)
}
