//! RI-5 CLI slice, CI-safe: an in-process S3-compatible mock (loopback only,
//! no credentials, no network beyond 127.0.0.1) stands in for the bucket.
//! It covers listing pagination, multi-object send/pull/retire, partial
//! failures, the declaration/credential plumbing and the status view.
//! The real-bucket round trip stays opt-in in `remote_inbox_r2_test.rs`.
#[path = "../src/test_support.rs"]
mod test_support;
use chat_stasher::{bundle_transport::BundleTransport, remote_inbox::*, send::send_bundle};
use ed25519_dalek::SigningKey;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{Arc, Mutex},
};
use test_support::Sandbox;

const BUNDLE: &[u8] = include_bytes!("../../../contracts/fixtures/inbox/cloud-account.json");
const BUCKET: &str = "synthetic-bucket";
const SECRET: &str = "synthetic-secret-value";
const KEY_ID: &str = "synthetic-access-id";

#[derive(Default)]
struct State {
    objects: BTreeMap<String, (Vec<u8>, i64)>,
    page_size: usize,
    list_calls: usize,
    /// Fail the n-th (1-based) list request with a 500.
    fail_list_call: Option<usize>,
    mtime: i64,
}
struct MockS3 {
    endpoint: String,
    state: Arc<Mutex<State>>,
}
impl MockS3 {
    fn start(page_size: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            page_size,
            mtime: 1_700_000_000,
            ..State::default()
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || serve(stream, &shared));
            }
        });
        Self { endpoint, state }
    }
    fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().objects.keys().cloned().collect()
    }
    fn put_raw(&self, key: &str, bytes: &[u8]) {
        let mut s = self.state.lock().unwrap();
        let mtime = s.mtime;
        s.objects.insert(key.into(), (bytes.to_vec(), mtime));
    }
    fn config(&self, prefix: &str) -> S3InboxConfig {
        S3InboxConfig {
            bucket: BUCKET.into(),
            endpoint: self.endpoint.clone(),
            access_key_id: KEY_ID.into(),
            secret_access_key: SECRET.into(),
            prefix: prefix.into(),
        }
    }
}
fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (
            parts.next().unwrap_or("").to_owned(),
            parts.next().unwrap_or("").to_owned(),
        );
        let (mut length, mut range) = (0usize, None);
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end().to_owned();
            if header.is_empty() {
                break;
            }
            let lower = header.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
            if let Some(v) = lower.strip_prefix("range: bytes=") {
                let (a, b) = v.split_once('-').unwrap();
                range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().ok()));
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let query: BTreeMap<String, String> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_owned(), unescape(v))
            })
            .collect();
        let key = unescape(path.trim_start_matches('/'));
        let key = key
            .strip_prefix(&format!("{BUCKET}/"))
            .unwrap_or("")
            .to_owned();
        let response = respond(state, &method, &key, &query, body, range);
        if out.write_all(&response).is_err() {
            return;
        }
    }
}
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
fn http(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}
fn respond(
    state: &Mutex<State>,
    method: &str,
    key: &str,
    query: &BTreeMap<String, String>,
    body: Vec<u8>,
    range: Option<(usize, Option<usize>)>,
) -> Vec<u8> {
    let mut s = state.lock().unwrap();
    let date = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .unwrap()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
    };
    if method == "GET" && query.contains_key("list-type") {
        s.list_calls += 1;
        if s.fail_list_call == Some(s.list_calls) {
            return http("500 Internal Server Error", &[], b"");
        }
        let prefix = query.get("prefix").cloned().unwrap_or_default();
        let delimiter = query.get("delimiter").cloned().unwrap_or_default();
        let start: usize = query
            .get("continuation-token")
            .map_or(0, |t| t.parse().unwrap());
        let limit = query
            .get("max-keys")
            .and_then(|v| v.parse().ok())
            .map_or(s.page_size, |m: usize| m.min(s.page_size));
        let mut contents = Vec::new();
        let mut dirs = std::collections::BTreeSet::new();
        for (k, (bytes, t)) in &s.objects {
            let Some(rest) = k.strip_prefix(&prefix) else {
                continue;
            };
            if !delimiter.is_empty() && rest.contains(&delimiter) {
                dirs.insert(format!("{prefix}{}/", rest.split('/').next().unwrap()));
            } else {
                contents.push((k.clone(), bytes.len(), *t));
            }
        }
        let page: Vec<_> = contents.iter().skip(start).take(limit).collect();
        let truncated = start + page.len() < contents.len();
        let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
        xml.push_str(&format!("<Name>{BUCKET}</Name><Prefix>{prefix}</Prefix>"));
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        if truncated {
            xml.push_str(&format!(
                "<NextContinuationToken>{}</NextContinuationToken>",
                start + page.len()
            ));
        }
        for (k, size, t) in page {
            let iso = chrono::DateTime::from_timestamp(*t, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S.000Z");
            xml.push_str(&format!(
                "<Contents><Key>{k}</Key><LastModified>{iso}</LastModified><ETag>\"e\"</ETag><Size>{size}</Size></Contents>"
            ));
        }
        for d in &dirs {
            xml.push_str(&format!(
                "<CommonPrefixes><Prefix>{d}</Prefix></CommonPrefixes>"
            ));
        }
        xml.push_str("</ListBucketResult>");
        return http(
            "200 OK",
            &[("content-type", "application/xml".into())],
            xml.as_bytes(),
        );
    }
    match method {
        "PUT" => {
            let mtime = s.mtime;
            s.objects.insert(key.into(), (body, mtime));
            http("200 OK", &[("etag", "\"e\"".into())], b"")
        }
        "DELETE" => {
            s.objects.remove(key);
            http("204 No Content", &[], b"")
        }
        "HEAD" | "GET" => {
            let Some((bytes, t)) = s.objects.get(key) else {
                return http("404 Not Found", &[], b"");
            };
            let common = [
                ("etag", "\"e\"".to_owned()),
                ("last-modified", date(*t)),
                ("accept-ranges", "bytes".to_owned()),
            ];
            if method == "HEAD" {
                // A HEAD response advertises the object length without a body.
                let mut head = http("200 OK", &common, b"");
                let text = String::from_utf8(head.clone()).unwrap();
                head = text
                    .replacen(
                        "content-length: 0",
                        &format!("content-length: {}", bytes.len()),
                        1,
                    )
                    .into_bytes();
                return head;
            }
            match range {
                Some((a, b)) => {
                    let end = b.map_or(bytes.len(), |b| (b + 1).min(bytes.len()));
                    let mut h = common.to_vec();
                    h.push((
                        "content-range",
                        format!("bytes {a}-{}/{}", end.saturating_sub(1), bytes.len()),
                    ));
                    http("206 Partial Content", &h, &bytes[a.min(end)..end])
                }
                None => http("200 OK", &common, bytes),
            }
        }
        _ => http("405 Method Not Allowed", &[], b""),
    }
}

fn open(mock: &MockS3, prefix: &str) -> RemoteInbox {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let entered = runtime.enter();
    let inbox = RemoteInbox::open_s3(&mock.config(prefix), 1024 * 1024).unwrap();
    drop(entered);
    // The blocking operator keeps its own handle to the runtime.
    std::mem::forget(runtime);
    inbox
}
fn signing() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn send_n(inbox: &RemoteInbox, identity: &age::x25519::Identity, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            send_bundle(
                inbox,
                BUNDLE,
                "synthetic-key",
                &identity.to_public(),
                &signing(),
            )
            .unwrap()
        })
        .collect()
}
fn policies() -> BTreeMap<String, KeyPolicy> {
    BTreeMap::from([(
        "synthetic-key".to_string(),
        KeyPolicy {
            public_key: signing().verifying_key(),
            platform: "synthetic-cloud".into(),
            expires_at: 100,
            revoked: false,
            max_bundle_bytes: 8192,
            max_objects_per_pull: 50,
        },
    )])
}
struct Proof(bool);
impl ArchiveProof for Proof {
    fn holds(&self, _: &str, _: &chat_stasher::inbox::SealOutcome) -> anyhow::Result<bool> {
        Ok(self.0)
    }
}

#[test]
fn listing_follows_every_page_of_a_large_prefix() {
    let mock = MockS3::start(3);
    let inbox = open(&mock, "inbox/paged");
    let identity = age::x25519::Identity::generate();
    let objects = send_n(&inbox, &identity, 8);
    // Objects of a sibling prefix and a nested directory are not this inbox's.
    mock.put_raw(&format!("inbox/other/{}", "a".repeat(64)), b"{}");
    mock.put_raw(&format!("inbox/paged/sub/{}", "b".repeat(64)), b"{}");
    let before = mock.state.lock().unwrap().list_calls;
    let listing = inbox.list().unwrap();
    let pages = mock.state.lock().unwrap().list_calls - before;
    assert_eq!(listing.items.len(), 8);
    assert_eq!(listing.total_inbox_files, 8);
    assert!(listing.errors.is_empty());
    assert!(
        pages >= 3,
        "8 objects at 3 per page need 3 requests, saw {pages}"
    );
    let mut expected = objects;
    expected.sort();
    let names: Vec<_> = listing.items.iter().map(|i| i.name.clone()).collect();
    assert_eq!(names, expected);
    // Status uses the same listing and the same completeness rule.
    let (count, oldest) = inbox.observe(1_700_000_100).unwrap();
    assert_eq!((count, oldest), (8, Some(100)));
}

#[test]
fn a_failed_page_is_an_unknown_count_never_a_short_listing() {
    let mock = MockS3::start(2);
    let inbox = open(&mock, "inbox/broken");
    let identity = age::x25519::Identity::generate();
    send_n(&inbox, &identity, 5);
    let calls = mock.state.lock().unwrap().list_calls;
    mock.state.lock().unwrap().fail_list_call = Some(calls + 2);
    let error = inbox.list().err().expect("second page fails").to_string();
    assert!(error.contains("waiting count unknown"));
    assert!(!error.contains(SECRET));
    mock.state.lock().unwrap().fail_list_call = Some(calls + 5);
    assert!(inbox.observe(0).is_err());
}

#[test]
fn many_objects_are_pulled_proven_and_retired_one_by_one() {
    let mock = MockS3::start(2);
    let inbox = open(&mock, "inbox/many");
    let identity = age::x25519::Identity::generate();
    send_n(&inbox, &identity, 5);
    // A forged object and a non-key name are refused without blocking the rest.
    let forged = serde_json::json!({"schema": "chat-stasher/remote-inbox@1", "key_id": "synthetic-key",
        "ciphertext": "AAAA", "signature": "AAAA"});
    mock.put_raw(&"c".repeat(64), b"unused");
    mock.put_raw(
        &format!("inbox/many/{}", "d".repeat(64)),
        forged.to_string().as_bytes(),
    );
    mock.put_raw("inbox/many/not-an-object-key", b"{}");
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("stage");
    let rates = root.path().join("rates.sqlite3");
    let run = |proof: bool| {
        pull(
            &inbox,
            &identity,
            &policies(),
            50,
            &stage,
            "synthetic-puller",
            100,
            &rates,
            &Proof(proof),
        )
        .unwrap()
    };
    // Without independent proof nothing is retired.
    let first = run(false);
    assert_eq!(first.waiting, 7);
    assert_eq!(
        first.stored + first.duplicates,
        5,
        "identical bundles dedupe in the sink"
    );
    assert_eq!(
        first
            .refused
            .iter()
            .filter(|r| **r == Refusal::Unproven)
            .count(),
        5
    );
    assert_eq!(
        mock.keys()
            .iter()
            .filter(|k| k.starts_with("inbox/many/"))
            .count(),
        7
    );
    // With proof the five good objects go; the two bad ones stay for review.
    let second = run(true);
    assert_eq!(second.waiting, 7);
    assert_eq!(second.refused.len(), 2);
    let left: Vec<_> = mock
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("inbox/many/"))
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert_eq!(
        second.exit_status(),
        3,
        "a non-key name is an unreadable item"
    );
    // The unrelated prefix was never touched.
    assert!(mock.keys().contains(&"c".repeat(64)));
}

fn cli(
    s: &Sandbox,
    mock: Option<&MockS3>,
    args: &[&str],
    token: Option<&str>,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    s.apply(&mut command)
        .env_remove("CHAT_STASHER_SEND_KEY")
        .env_remove(S3_ENDPOINT_ENV)
        .env_remove(S3_ACCESS_KEY_ID_ENV)
        .env_remove(S3_SECRET_ACCESS_KEY_ENV);
    if let Some(mock) = mock {
        command
            .env(S3_ENDPOINT_ENV, &mock.endpoint)
            .env(S3_ACCESS_KEY_ID_ENV, KEY_ID)
            .env(S3_SECRET_ACCESS_KEY_ENV, SECRET);
    }
    if let Some(token) = token {
        command.env("CHAT_STASHER_SEND_KEY", token);
    }
    command.args(args).output().unwrap()
}
fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
fn no_secret(output: &std::process::Output) {
    let all = text(output);
    assert!(
        !all.contains(SECRET) && !all.contains(KEY_ID),
        "credential leaked"
    );
}

struct Cli {
    owner: Sandbox,
    producer: Sandbox,
    token: String,
}
fn cli_fixture() -> Cli {
    let owner = Sandbox::new();
    let producer = Sandbox::new();
    let init = cli(
        &owner,
        None,
        &[
            "inbox-init",
            "synthetic-inbox",
            "--locator",
            &format!("s3://{BUCKET}/inbox/cli"),
        ],
        None,
    );
    assert!(init.status.success(), "{}", text(&init));
    // The declaration holds no credential: only the locator.
    let declaration = std::fs::read_to_string(
        owner
            .config_home()
            .join("chat-stasher/inboxes/synthetic-inbox.json"),
    )
    .unwrap();
    assert!(declaration.contains("s3://synthetic-bucket/inbox/cli"));
    let credential = owner.root().join("synthetic-credential.json");
    std::fs::write(&credential, br#"{"token":"synthetic-posting-secret"}"#).unwrap();
    let issued = cli(
        &owner,
        None,
        &[
            "send-key",
            "synthetic-inbox",
            "--for",
            "synthetic-cloud",
            "--credential-file",
            credential.to_str().unwrap(),
        ],
        None,
    );
    assert!(issued.status.success(), "{}", text(&issued));
    Cli {
        owner,
        producer,
        token: String::from_utf8(issued.stdout).unwrap().trim().to_owned(),
    }
}

#[test]
fn inbox_init_accepts_only_a_dedicated_bucket_prefix_for_s3() {
    let s = Sandbox::new();
    for bad in [
        "s3://bucket",
        "s3://bucket/",
        "s3:///prefix",
        "s3://bucket/a/../b",
        "s3://bucket//x",
        "s3://key:secret@bucket/p",
        "s3://bucket/p?x=1",
    ] {
        let r = cli(
            &s,
            None,
            &["inbox-init", "synthetic-inbox", "--locator", bad],
            None,
        );
        assert_eq!(r.status.code(), Some(2), "{bad}");
        assert!(
            !s.config_home().join("chat-stasher/inboxes").exists(),
            "{bad}"
        );
        no_secret(&r);
    }
    let ok = cli(
        &s,
        None,
        &[
            "inbox-init",
            "synthetic-inbox",
            "--locator",
            "s3://bucket/dedicated",
        ],
        None,
    );
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
}

#[test]
fn cli_send_then_status_then_pull_over_s3_with_many_objects() {
    let mock = MockS3::start(2);
    let f = cli_fixture();
    let input = f.producer.root().join("synthetic-transcript.jsonl");
    for n in 0..5 {
        std::fs::write(&input, format!("{{\"n\":{n}}}\n")).unwrap();
        let sent = cli(
            &f.producer,
            Some(&mock),
            &[
                "send",
                "--path",
                input.to_str().unwrap(),
                "--session",
                "synthetic-native",
            ],
            Some(&f.token),
        );
        assert_eq!(sent.status.code(), Some(0), "{}", text(&sent));
        assert_eq!(sent.stdout, b"send complete: uploaded=1; resumed=0\n");
        no_secret(&sent);
    }
    let keys = mock.keys();
    assert_eq!(keys.len(), 5);
    assert!(keys
        .iter()
        .all(|k| k.starts_with("inbox/cli/") && k.len() == "inbox/cli/".len() + 64));
    // A repeat resumes from the local receipt and uploads nothing.
    let again = cli(
        &f.producer,
        Some(&mock),
        &[
            "send",
            "--path",
            input.to_str().unwrap(),
            "--session",
            "synthetic-native",
        ],
        Some(&f.token),
    );
    assert_eq!(again.stdout, b"send complete: uploaded=0; resumed=1\n");
    assert_eq!(mock.keys().len(), 5);

    // Status: a paged listing, an exact count, the s3 label, no leak.
    let status = cli(&f.owner, Some(&mock), &["status", "--json"], None);
    no_secret(&status);
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let inbox = &value["remote_inboxes"]["inboxes"][0];
    assert_eq!(inbox["waiting"], 5);
    assert_eq!(inbox["backend"], "s3");
    assert_eq!(inbox["backend_state"], "known");
    assert!(inbox["oldest_age_secs"].is_u64());
    // Without credentials the count is unknown, not zero, and nothing is dialled.
    let blind = cli(&f.owner, None, &["status", "--json"], None);
    let value: serde_json::Value = serde_json::from_slice(&blind.stdout).unwrap();
    let inbox = &value["remote_inboxes"]["inboxes"][0];
    assert!(inbox["waiting"].is_null());
    assert_eq!(inbox["backend_state"], "unknown");

    // Pull: all five listed and sealed; no destination proof is wired in this
    // fixture so exit 3 and every object stays, exactly like the fs path.
    let stage = f.owner.root().join("synthetic-stage");
    let pull_args = [
        "inbox-pull",
        "synthetic-inbox",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "synthetic-puller",
    ];
    let pulled = cli(&f.owner, Some(&mock), &pull_args, None);
    no_secret(&pulled);
    assert!(
        String::from_utf8_lossy(&pulled.stdout).contains("waiting=5; stored=5"),
        "{}",
        text(&pulled)
    );
    assert_eq!(pulled.status.code(), Some(3));
    assert_eq!(mock.keys().len(), 5);
    // Missing credentials: incomplete (3), never an empty inbox, nothing sealed.
    let no_stage = f.owner.root().join("synthetic-stage-2");
    let blind = cli(
        &f.owner,
        None,
        &[
            "inbox-pull",
            "synthetic-inbox",
            "--stage",
            no_stage.to_str().unwrap(),
            "--machine",
            "synthetic-puller",
        ],
        None,
    );
    assert_eq!(blind.status.code(), Some(3));
    assert!(text(&blind).contains("credential environment invalid"));
    assert!(!no_stage.exists());
}

#[test]
fn cli_send_refuses_without_credentials_or_over_plain_http_to_a_remote_host() {
    let f = cli_fixture();
    let input = f.producer.root().join("synthetic-transcript.jsonl");
    std::fs::write(&input, b"{}\n").unwrap();
    let args = [
        "send",
        "--path",
        input.to_str().unwrap(),
        "--session",
        "synthetic-native",
    ];
    let none = cli(&f.producer, None, &args, Some(&f.token));
    assert_eq!(none.status.code(), Some(1));
    assert!(text(&none).contains("credential environment invalid"));
    assert!(!f
        .producer
        .state_home()
        .join("chat-stasher/send-cursors")
        .exists());
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    f.producer.apply(&mut command);
    let remote = command
        .env("CHAT_STASHER_SEND_KEY", &f.token)
        .env(S3_ENDPOINT_ENV, "http://storage.example.invalid")
        .env(S3_ACCESS_KEY_ID_ENV, KEY_ID)
        .env(S3_SECRET_ACCESS_KEY_ENV, SECRET)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(remote.status.code(), Some(1));
    no_secret(&remote);
}

#[test]
fn an_unreachable_bucket_is_an_incomplete_send_and_an_unknown_status() {
    let f = cli_fixture();
    // Port 9 (discard) has no listener on a test host.
    let unreachable = MockS3 {
        endpoint: "http://127.0.0.1:9".into(),
        state: Arc::default(),
    };
    let input = f.producer.root().join("synthetic-transcript.jsonl");
    std::fs::write(&input, b"{}\n").unwrap();
    let sent = cli(
        &f.producer,
        Some(&unreachable),
        &[
            "send",
            "--path",
            input.to_str().unwrap(),
            "--session",
            "synthetic-native",
        ],
        Some(&f.token),
    );
    assert_eq!(sent.status.code(), Some(3));
    no_secret(&sent);
    assert!(
        !f.producer
            .state_home()
            .join("chat-stasher/send-cursors")
            .exists(),
        "no receipt without an upload"
    );
    let status = cli(&f.owner, Some(&unreachable), &["status", "--json"], None);
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(value["remote_inboxes"]["inboxes"][0]["waiting"].is_null());
    assert_eq!(
        value["remote_inboxes"]["inboxes"][0]["backend_state"],
        "unknown"
    );
}

#[test]
fn s3_locator_and_environment_parsing_never_echoes_values() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let full: &[(&str, &str)] = &[
        (S3_ENDPOINT_ENV, "https://synthetic.invalid"),
        (S3_ACCESS_KEY_ID_ENV, KEY_ID),
        (S3_SECRET_ACCESS_KEY_ENV, SECRET),
    ];
    let c = S3InboxConfig::from_locator("s3://b/p/q", env(full)).unwrap();
    assert_eq!((c.bucket.as_str(), c.prefix.as_str()), ("b", "p/q"));
    assert!(!format!("{c:?}").contains(SECRET));
    let partial: &[(&str, &str)] = &[
        (S3_ENDPOINT_ENV, "https://synthetic.invalid"),
        (S3_ACCESS_KEY_ID_ENV, ""),
    ];
    let error = S3InboxConfig::from_locator("s3://b/p", env(partial))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(S3_SECRET_ACCESS_KEY_ENV) && !error.contains("synthetic.invalid"));
    assert!(S3InboxConfig::from_locator("s3://b", env(full)).is_err());
    assert!(S3InboxConfig::from_locator("fs:///x", env(full)).is_err());
}

#[test]
fn plain_http_is_accepted_only_for_a_true_loopback_host() {
    let get = |name: &str| match name {
        S3_ENDPOINT_ENV => Some(ENDPOINT.with(|e| e.borrow().clone())),
        S3_ACCESS_KEY_ID_ENV => Some(KEY_ID.to_owned()),
        S3_SECRET_ACCESS_KEY_ENV => Some(SECRET.to_owned()),
        _ => None,
    };
    let accepted = |endpoint: &str| {
        ENDPOINT.with(|e| *e.borrow_mut() = endpoint.to_owned());
        S3InboxConfig::from_locator("s3://b/p", get).is_ok()
    };
    for good in [
        "http://127.0.0.1:9000",
        "http://localhost",
        "http://localhost:9000/",
        "http://[::1]:9000",
        "http://[::1]",
        "https://account.r2.cloudflarestorage.com",
    ] {
        assert!(accepted(good), "{good}");
    }
    for bad in [
        "http://localhost:80@evil.example",
        "http://127.0.0.1:80@evil.example",
        "http://[::1]@evil.example",
        "http://[::1]:80@evil.example/",
        "http://user:pw@127.0.0.1:9000",
        "http://127.0.0.1.evil.example",
        "http://localhost.evil.example",
        "http://localhostevil.example",
        "http://127.0.0.1:80x",
        "http://127.0.0.1:",
        "http://[::1]x",
        "http://[::1",
        "http://evil.example/?@localhost",
        "http://evil.example#@127.0.0.1",
        "http://evil.example\\@localhost",
        "http://localhost\\@evil.example",
        "http://",
        "http:// localhost",
        "http://storage.example.invalid",
        "ftp://localhost",
    ] {
        assert!(!accepted(bad), "{bad}");
    }
}
thread_local! {
    static ENDPOINT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}
