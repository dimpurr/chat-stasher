//! RI-5 real-backend slice: the S3/R2 inbox opening and a destination-backed
//! send -> object storage -> pull -> archive proof -> retire round trip.
//!
//! CI runs only the credential-free tests. The live test is opt-in: it needs
//! `CHAT_STASHER_R2_TEST_LIVE=1` plus the `CHAT_STASHER_R2_TEST_{BUCKET,
//! ENDPOINT,ACCESS_KEY_ID,SECRET_ACCESS_KEY}` variables, refuses any bucket
//! other than the dedicated test bucket, writes only under a unique prefix it
//! creates, and deletes only that prefix. No value is ever printed.
use chat_stasher::{
    bundle_transport::BundleTransport,
    remote_inbox::*,
    remote_inbox_archive::DestinationArchive,
    send::send_bundle,
    store::{self, BackupStore, StoreConfig},
};
use ed25519_dalek::SigningKey;
use std::collections::BTreeMap;

const TEST_BUCKET: &str = "chat-stasher-inbox-test";
const BUNDLE: &[u8] = include_bytes!("../../../contracts/fixtures/inbox/cloud-account.json");

fn cfg(prefix: &str) -> S3InboxConfig {
    S3InboxConfig {
        bucket: "synthetic-bucket".into(),
        endpoint: "https://synthetic.invalid".into(),
        access_key_id: "synthetic-access-id".into(),
        secret_access_key: "synthetic-secret-value".into(),
        prefix: prefix.into(),
    }
}

#[test]
fn s3_inbox_refuses_root_traversal_and_incomplete_config_without_network() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    for bad in ["", "/", "//", "a/../b", "a/./b", "a//b", ".."] {
        assert!(
            RemoteInbox::open_s3(&cfg(bad), 1024).is_err(),
            "prefix {bad:?}"
        );
    }
    let mut incomplete = cfg("ok");
    incomplete.bucket.clear();
    assert!(RemoteInbox::open_s3(&incomplete, 1024).is_err());
    // A well-formed config opens lazily: no request is made until first use.
    assert!(RemoteInbox::open_s3(&cfg("inbox/one"), 1024).is_ok());
}

#[test]
fn s3_inbox_config_debug_never_shows_credentials() {
    let text = format!("{:?}", cfg("p"));
    assert!(!text.contains("synthetic-secret-value"));
    assert!(!text.contains("synthetic-access-id"));
    assert!(text.contains("redacted"));
}

#[test]
fn s3_inbox_unreachable_endpoint_is_unknown_never_empty() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let mut c = cfg("inbox/unreachable");
    c.endpoint = "http://127.0.0.1:9".into();
    let remote = RemoteInbox::open_s3(&c, 1024).unwrap();
    // A failed listing is an error, never an empty successful pass.
    let err = remote.list().err().expect("listing must fail").to_string();
    assert!(err.contains("waiting count unknown"));
    assert!(!err.contains("synthetic-secret-value"));
}

struct Purge<'a>(&'a RemoteInbox);
impl Drop for Purge<'_> {
    fn drop(&mut self) {
        // Own unique prefix only (the operator root); fixed-phrase on failure.
        if self.0.purge_flat().is_err() {
            eprintln!("live test cleanup incomplete: check the test prefix");
        }
    }
}

#[test]
fn live_r2_round_trip_with_destination_backed_proof() {
    if std::env::var("CHAT_STASHER_R2_TEST_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set CHAT_STASHER_R2_TEST_LIVE=1 with the test bucket variables");
        return;
    }
    let var = |k: &str| std::env::var(format!("CHAT_STASHER_R2_TEST_{k}")).unwrap_or_default();
    // Bucket check before any write or delete.
    assert_eq!(
        var("BUCKET"),
        TEST_BUCKET,
        "refusing: not the dedicated test bucket"
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config = S3InboxConfig {
        bucket: var("BUCKET"),
        endpoint: var("ENDPOINT"),
        access_key_id: var("ACCESS_KEY_ID"),
        secret_access_key: var("SECRET_ACCESS_KEY"),
        prefix: format!("ri5-live-{}-{nanos}", std::process::id()),
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let entered = runtime.enter();
    let remote = RemoteInbox::open_s3(&config, 1024 * 1024).unwrap();
    drop(entered);
    let _purge = Purge(&remote);
    assert!(
        remote.list().unwrap().items.is_empty(),
        "fresh prefix must be empty"
    );

    let identity = age::x25519::Identity::generate();
    let signing = SigningKey::from_bytes(&[7; 32]);
    let policies = BTreeMap::from([(
        "synthetic-key".to_string(),
        KeyPolicy {
            public_key: signing.verifying_key(),
            platform: "synthetic-cloud".into(),
            expires_at: 100,
            revoked: false,
            max_bundle_bytes: 8192,
            max_objects_per_pull: 10,
        },
    )]);
    let object = send_bundle(
        &remote,
        BUNDLE,
        "synthetic-key",
        &identity.to_public(),
        &signing,
    )
    .unwrap();
    assert_eq!(remote.list().unwrap().items.len(), 1);
    let stored = remote.fetch(&object).unwrap();
    assert!(
        !stored.windows(16).any(|s| s == b"synthetic-native"),
        "object at rest must be ciphertext"
    );

    // A real rustic destination (local repo), independent of the stage.
    let root = tempfile::Builder::new()
        .prefix("ri5-live-")
        .tempdir()
        .unwrap();
    let dest = StoreConfig {
        repo_root: root.path().join("dest").to_string_lossy().into_owned(),
        key_file: root.path().join("dest.key"),
        connections: 1,
        cache_dir: Some(root.path().join("dest-cache")),
        ..Default::default()
    };
    let key = rustic_core::repofile::MasterKey::new();
    store::persist_key_file(&dest, &key).unwrap();
    BackupStore::new(dest.clone(), "synthetic-puller".into())
        .open_or_init(&key)
        .unwrap();
    let destinations = [dest.clone()];
    let stage = root.path().join("stage");
    let rates = root.path().join("rates.sqlite3");
    let archive = DestinationArchive::new(&stage, &destinations);
    let pull = || {
        chat_stasher::remote_inbox::pull(
            &remote,
            &identity,
            &policies,
            50,
            &stage,
            "synthetic-puller",
            100,
            &rates,
            &archive,
        )
        .unwrap()
    };

    // Sealed in the stage but not yet in the destination: unproven, kept.
    let first = pull();
    assert_eq!(first.stored, 1);
    assert_eq!(first.refused, vec![Refusal::Unproven]);
    assert_eq!(
        remote.list().unwrap().items.len(),
        1,
        "unproven object must wait"
    );

    // Push to the destination; the resend is a duplicate and now provable.
    BackupStore::new(dest.clone(), "synthetic-puller".into())
        .push(&stage, &store::load_key_file(&dest).unwrap())
        .unwrap();
    let second = pull();
    assert!(second.refused.is_empty());
    assert_eq!(second.duplicates, 1);
    assert_eq!(second.exit_status(), 0);
    assert!(
        remote.list().unwrap().items.is_empty(),
        "proven object retired"
    );
}
