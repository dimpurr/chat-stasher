//! Synthetic send → object storage → pull → durable sink regression coverage.
use chat_stasher::{bundle_transport::BundleTransport, remote_inbox::*, send::send_bundle};
use ed25519_dalek::SigningKey;
use std::collections::BTreeMap;

/// An independent synthetic archive: copy the sealed body into another root,
/// then read it back and compare every byte before allowing retirement.
struct SyntheticArchive<'a> {
    stage: &'a std::path::Path,
    root: tempfile::TempDir,
}
impl ArchiveProof for SyntheticArchive<'_> {
    fn holds(
        &self,
        machine: &str,
        outcome: &chat_stasher::inbox::SealOutcome,
    ) -> anyhow::Result<bool> {
        let (id, shard, expected) = match outcome {
            chat_stasher::inbox::SealOutcome::Stored(row) => {
                (&row.id, &row.shard, &row.file_sha256)
            }
            chat_stasher::inbox::SealOutcome::Duplicate(row) => {
                (&row.id, &row.matched_shard, &row.file_sha256)
            }
        };
        let dir = chat_stasher::store::session_shard_dir(self.stage, machine, id);
        let path = chat_stasher::store::sealed_shard_entries(&dir)?
            .into_iter()
            .map(|(_, p)| p)
            .find(|p| p.file_name().unwrap() == shard.as_str())
            .unwrap();
        let sealed = std::fs::read(path)?;
        let copy = self.root.path().join("synthetic-archived-body");
        std::fs::write(&copy, &sealed)?;
        let archived = std::fs::read(copy)?;
        let record: serde_json::Value = serde_json::from_slice(&archived)?;
        Ok(archived == sealed && record["file_sha256"] == expected.as_str())
    }
}
fn pull<T: BundleTransport>(
    transport: &T,
    identity: &age::x25519::Identity,
    keys: &BTreeMap<String, KeyPolicy>,
    now: u64,
    stage: &std::path::Path,
    machine: &str,
    cap: usize,
) -> anyhow::Result<PullReport> {
    let archive = SyntheticArchive {
        stage,
        root: tempfile::Builder::new()
            .prefix("w713-remote-inbox-proof-")
            .tempdir()?,
    };
    chat_stasher::remote_inbox::pull(
        transport,
        identity,
        keys,
        now,
        stage,
        machine,
        cap,
        &archive.root.path().join("synthetic-rates.sqlite3"),
        &archive,
    )
}

fn transport() -> (tokio::runtime::Runtime, RemoteInbox) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let op = opendal::Operator::new(opendal::services::Memory::default())
        .unwrap()
        .finish();
    let remote = RemoteInbox::new(opendal::blocking::Operator::new(op).unwrap(), 1024 * 1024);
    drop(_entered);
    (runtime, remote)
}
fn keys() -> (
    age::x25519::Identity,
    SigningKey,
    BTreeMap<String, KeyPolicy>,
) {
    let identity = age::x25519::Identity::generate();
    let signing = SigningKey::from_bytes(&[7; 32]);
    let policy = KeyPolicy {
        public_key: signing.verifying_key(),
        platform: "synthetic-cloud".into(),
        expires_at: 100,
        revoked: false,
        max_bundle_bytes: 8192,
        max_objects_per_pull: 10,
    };
    (
        identity,
        signing,
        BTreeMap::from([("synthetic-key".into(), policy)]),
    )
}
const BUNDLE: &[u8] = include_bytes!("../../../contracts/fixtures/inbox/cloud-account.json");
#[test]
fn remote_round_trip_and_resend_prove_bytes_before_retirement() {
    let (_runtime, remote) = transport();
    let (identity, signing, policies) = keys();
    let stage = tempfile::Builder::new()
        .prefix("w713-remote-inbox-")
        .tempdir()
        .unwrap();
    for duplicate in [false, true] {
        let object = send_bundle(
            &remote,
            BUNDLE,
            "synthetic-key",
            &identity.to_public(),
            &signing,
        )
        .unwrap();
        assert_eq!(object.len(), 64);
        assert!(!remote
            .fetch(&object)
            .unwrap()
            .windows(16)
            .any(|s| s == b"synthetic-native"));
        let report = pull(
            &remote,
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100,
        )
        .unwrap();
        assert!(report.refused.is_empty());
        assert_eq!(report.stored, usize::from(!duplicate));
        assert_eq!(report.duplicates, usize::from(duplicate));
        assert!(remote.list().unwrap().items.is_empty());
    }
    let shard = chat_stasher::store::shard_path_with_cap(
        stage.path(),
        "synthetic-puller",
        "claude-code.synthetic-account.synthetic-native",
        1,
        100,
    );
    let row: serde_json::Value = serde_json::from_slice(&std::fs::read(shard).unwrap()).unwrap();
    assert_eq!(row["raw"]["data"], "{}\n");
    assert_eq!(row["producer"]["sendKeyId"], "synthetic-key");
}
#[test]
fn remote_refusals_keep_waiting_objects_and_stage_untouched() {
    for reason in [
        Refusal::UnknownKey,
        Refusal::RevokedKey,
        Refusal::ExpiredKey,
        Refusal::SizeLimit,
        Refusal::RateLimit,
        Refusal::Signature,
        Refusal::Decrypt,
    ] {
        let (_runtime, remote) = transport();
        let (identity, signing, mut policies) = keys();
        send_bundle(
            &remote,
            BUNDLE,
            "synthetic-key",
            &identity.to_public(),
            &signing,
        )
        .unwrap();
        match reason {
            Refusal::UnknownKey => policies.clear(),
            Refusal::RevokedKey => policies.get_mut("synthetic-key").unwrap().revoked = true,
            Refusal::ExpiredKey => policies.get_mut("synthetic-key").unwrap().expires_at = 50,
            Refusal::SizeLimit => policies.get_mut("synthetic-key").unwrap().max_bundle_bytes = 1,
            Refusal::RateLimit => {
                policies
                    .get_mut("synthetic-key")
                    .unwrap()
                    .max_objects_per_pull = 0
            }
            Refusal::Signature => {
                policies.get_mut("synthetic-key").unwrap().public_key =
                    SigningKey::from_bytes(&[8; 32]).verifying_key()
            }
            _ => {}
        }
        let wrong = age::x25519::Identity::generate();
        let stage = tempfile::Builder::new()
            .prefix("w713-remote-inbox-")
            .tempdir()
            .unwrap();
        let report = pull(
            &remote,
            if reason == Refusal::Decrypt {
                &wrong
            } else {
                &identity
            },
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100,
        )
        .unwrap();
        assert_eq!(report.refused, vec![reason]);
        assert_eq!(remote.list().unwrap().items.len(), 1);
        assert_eq!(std::fs::read_dir(stage.path()).unwrap().count(), 0);
    }
}

struct FailingTransport<'a> {
    remote: &'a RemoteInbox,
    list: bool,
    fetch: bool,
    retire: bool,
}
impl BundleTransport for FailingTransport<'_> {
    type Item = String;
    fn list(&self) -> anyhow::Result<chat_stasher::bundle_transport::BundleListing<String>> {
        if self.list {
            anyhow::bail!("synthetic unavailable");
        }
        self.remote.list()
    }
    fn fetch(&self, item: &String) -> anyhow::Result<Vec<u8>> {
        if self.fetch {
            anyhow::bail!("synthetic unavailable");
        }
        self.remote.fetch(item)
    }
    fn retire(&self, name: &str, item: &String) -> anyhow::Result<()> {
        if self.retire {
            anyhow::bail!("synthetic unavailable");
        }
        self.remote.retire(name, item)
    }
}
#[test]
fn unknown_listing_fetch_and_failed_retirement_are_retryable() {
    let (_runtime, remote) = transport();
    let (identity, signing, policies) = keys();
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let stage = tempfile::Builder::new()
        .prefix("w713-remote-inbox-")
        .tempdir()
        .unwrap();
    for (list, fetch, retire) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let broken = FailingTransport {
            remote: &remote,
            list,
            fetch,
            retire,
        };
        let result = pull(
            &broken,
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100,
        );
        if list {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result.unwrap().refused,
                vec![if fetch {
                    Refusal::Fetch
                } else {
                    Refusal::Retire
                }]
            );
        }
        assert_eq!(remote.list().unwrap().items.len(), 1);
    }
    let retry = pull(
        &remote,
        &identity,
        &policies,
        50,
        stage.path(),
        "synthetic-puller",
        100,
    )
    .unwrap();
    assert_eq!(retry.duplicates, 1);
    assert!(remote.list().unwrap().items.is_empty());
}
#[test]
fn scope_mismatch_and_sink_failure_never_retire() {
    let (_runtime, remote) = transport();
    let (identity, signing, mut policies) = keys();
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let stage = tempfile::Builder::new()
        .prefix("w713-remote-inbox-")
        .tempdir()
        .unwrap();
    policies.get_mut("synthetic-key").unwrap().platform = "other-platform".into();
    assert_eq!(
        pull(
            &remote,
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100
        )
        .unwrap()
        .refused,
        vec![Refusal::Scope]
    );
    policies.get_mut("synthetic-key").unwrap().platform = "synthetic-cloud".into();
    let blocked = stage.path().join("blocked");
    std::fs::write(&blocked, b"synthetic").unwrap();
    assert_eq!(
        pull(
            &remote,
            &identity,
            &policies,
            50,
            &blocked,
            "synthetic-puller",
            100
        )
        .unwrap()
        .refused,
        vec![Refusal::Seal]
    );
    assert_eq!(remote.list().unwrap().items.len(), 1);
}

#[test]
fn explicit_binary_path_records_unknown_fidelity_without_local_identity() {
    let (_runtime, remote) = transport();
    let (identity, signing, policies) = keys();
    let root = tempfile::Builder::new()
        .prefix("w713-remote-inbox-")
        .tempdir()
        .unwrap();
    let input = root.path().join("synthetic.bin");
    let body = [0, 255, 13, 10];
    std::fs::write(&input, body).unwrap();
    chat_stasher::send::send_path(
        &remote,
        &input,
        "synthetic-session",
        "synthetic-cloud",
        "synthetic-key",
        &identity.to_public(),
        &signing,
        1024,
    )
    .unwrap();
    let stage = root.path().join("stage");
    let report = pull(
        &remote,
        &identity,
        &policies,
        50,
        &stage,
        "synthetic-puller",
        100,
    )
    .unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.stored, 1);
    let row: serde_json::Value = serde_json::from_slice(
        &std::fs::read(chat_stasher::store::shard_path_with_cap(
            &stage,
            "synthetic-puller",
            "unknown.synthetic-session",
            1,
            100,
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(row["fidelity"]["value"], "unknown");
    assert_eq!(row["file"]["relPath"], "synthetic.bin");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(row["raw"]["data"].as_str().unwrap())
            .unwrap(),
        body
    );
}

struct Tampered<'a>(&'a RemoteInbox);
impl BundleTransport for Tampered<'_> {
    type Item = String;
    fn list(&self) -> anyhow::Result<chat_stasher::bundle_transport::BundleListing<String>> {
        self.0.list()
    }
    fn fetch(&self, item: &String) -> anyhow::Result<Vec<u8>> {
        use base64::Engine;
        let mut envelope: serde_json::Value = serde_json::from_slice(&self.0.fetch(item)?)?;
        let engine = base64::engine::general_purpose::STANDARD;
        let mut ciphertext = engine.decode(envelope["ciphertext"].as_str().unwrap())?;
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 1;
        envelope["ciphertext"] = engine.encode(ciphertext).into();
        Ok(serde_json::to_vec(&envelope)?)
    }
    fn retire(&self, _: &str, _: &String) -> anyhow::Result<()> {
        panic!("tampered object must never retire")
    }
}
#[test]
fn ciphertext_tampering_is_refused_before_sealing() {
    let (_runtime, remote) = transport();
    let (identity, signing, policies) = keys();
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let stage = tempfile::Builder::new()
        .prefix("w713-remote-inbox-")
        .tempdir()
        .unwrap();
    assert_eq!(
        pull(
            &Tampered(&remote),
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100
        )
        .unwrap()
        .refused,
        vec![Refusal::Signature]
    );
    assert_eq!(std::fs::read_dir(stage.path()).unwrap().count(), 0);
    assert_eq!(remote.list().unwrap().items.len(), 1);
}
#[test]
fn invalid_capture_is_never_uploaded() {
    let (_runtime, remote) = transport();
    let (identity, signing, _) = keys();
    assert!(send_bundle(
        &remote,
        b"{}",
        "synthetic-key",
        &identity.to_public(),
        &signing
    )
    .is_err());
    assert!(send_bundle(
        &remote,
        BUNDLE,
        "other-key",
        &identity.to_public(),
        &signing
    )
    .is_err());
    assert!(remote.list().unwrap().items.is_empty());
}

struct UnprovenArchive {
    unreadable: bool,
}
impl ArchiveProof for UnprovenArchive {
    fn holds(&self, _: &str, _: &chat_stasher::inbox::SealOutcome) -> anyhow::Result<bool> {
        if self.unreadable {
            anyhow::bail!("synthetic archive unreadable");
        }
        Ok(false)
    }
}
#[test]
fn durable_stage_without_archive_proof_never_retires() {
    for unreadable in [false, true] {
        let (_runtime, remote) = transport();
        let (identity, signing, policies) = keys();
        send_bundle(
            &remote,
            BUNDLE,
            "synthetic-key",
            &identity.to_public(),
            &signing,
        )
        .unwrap();
        let stage = tempfile::Builder::new()
            .prefix("w713-remote-inbox-")
            .tempdir()
            .unwrap();
        let report = chat_stasher::remote_inbox::pull(
            &remote,
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100,
            &stage.path().join("synthetic-rates.sqlite3"),
            &UnprovenArchive { unreadable },
        )
        .unwrap();
        assert_eq!(report.stored, 1);
        assert_eq!(report.refused, vec![Refusal::Unproven]);
        assert_eq!(remote.list().unwrap().items.len(), 1);
        let retry = pull(
            &remote,
            &identity,
            &policies,
            50,
            stage.path(),
            "synthetic-puller",
            100,
        )
        .unwrap();
        assert_eq!(retry.duplicates, 1);
        assert!(remote.list().unwrap().items.is_empty());
    }
}

#[test]
fn rolling_rate_survives_pull_restarts_and_expires_at_the_boundary() {
    let (_runtime, remote) = transport();
    let (identity, signing, mut policies) = keys();
    let policy = policies.get_mut("synthetic-key").unwrap();
    policy.max_objects_per_pull = 1;
    policy.expires_at = 10000;
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("synthetic-stage");
    let archive = UnprovenArchive { unreadable: false };
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let first = chat_stasher::remote_inbox::pull(
        &remote,
        &identity,
        &policies,
        50,
        &stage,
        "synthetic-puller",
        100,
        &root.path().join("synthetic-rates.sqlite3"),
        &archive,
    )
    .unwrap();
    assert_eq!(first.refused, [Refusal::Unproven]);
    for now in [50, 51, 3649] {
        let retry = chat_stasher::remote_inbox::pull(
            &remote,
            &identity,
            &policies,
            now,
            &stage,
            "synthetic-puller",
            100,
            &root.path().join("synthetic-rates.sqlite3"),
            &archive,
        )
        .unwrap();
        assert_eq!(retry.refused, [Refusal::RateLimit]);
        assert_eq!(retry.stored + retry.duplicates, 0);
        assert_eq!(remote.list().unwrap().items.len(), 1);
    }
    let boundary = chat_stasher::remote_inbox::pull(
        &remote,
        &identity,
        &policies,
        3650,
        &stage,
        "synthetic-puller",
        100,
        &root.path().join("synthetic-rates.sqlite3"),
        &archive,
    )
    .unwrap();
    assert_eq!(boundary.duplicates, 1);
    assert_eq!(boundary.refused, [Refusal::Unproven]);
}

#[test]
fn unavailable_rate_accounting_keeps_objects_and_never_seals() {
    let (_runtime, remote) = transport();
    let (identity, signing, policies) = keys();
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("synthetic-stage");
    let state = root.path().join("synthetic-rates.sqlite3");
    std::fs::write(&state, b"synthetic-corruption").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let result = chat_stasher::remote_inbox::pull(
        &remote,
        &identity,
        &policies,
        50,
        &stage,
        "synthetic-puller",
        100,
        &state,
        &UnprovenArchive { unreadable: false },
    )
    .unwrap();
    assert_eq!(result.refused, [Refusal::RateAccounting]);
    assert_eq!(result.stored + result.duplicates, 0);
    assert!(!stage.exists());
    assert_eq!(remote.list().unwrap().items.len(), 1);
    assert_eq!(std::fs::read(&state).unwrap(), b"synthetic-corruption");
}

#[test]
fn invalid_signature_cannot_consume_another_keys_quota() {
    let (_runtime, remote) = transport();
    let (identity, signing, mut policies) = keys();
    policies
        .get_mut("synthetic-key")
        .unwrap()
        .max_objects_per_pull = 1;
    send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("synthetic-stage");
    let state = root.path().join("synthetic-rates.sqlite3");
    let archive = UnprovenArchive { unreadable: false };
    let invalid = chat_stasher::remote_inbox::pull(
        &Tampered(&remote),
        &identity,
        &policies,
        50,
        &stage,
        "synthetic-puller",
        100,
        &state,
        &archive,
    )
    .unwrap();
    assert_eq!(invalid.refused, [Refusal::Signature]);
    assert!(!state.exists());
    assert!(!stage.exists());
    let valid = chat_stasher::remote_inbox::pull(
        &remote,
        &identity,
        &policies,
        50,
        &stage,
        "synthetic-puller",
        100,
        &state,
        &archive,
    )
    .unwrap();
    assert_eq!(valid.stored, 1);
    assert_eq!(valid.refused, [Refusal::Unproven]);
}
