//! Bounded Letta API producer. Local inbox publication and archive proof are
//! separate: only destination proof commits the per-account, per-agent cursor.
use crate::remote_inbox::ArchiveProof;
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(clap::Args)]
pub struct PullArgs {
    /// Provider (currently letta).
    #[arg(value_parser = ["letta"])]
    pub provider: String,
    /// Stable provider account ID, independent of the API key. Never logged.
    #[arg(long)]
    pub account_id: String,
    /// Local inbox for ordinary @3 harness-file bundles.
    #[arg(long)]
    pub inbox: PathBuf,
    /// Durable private producer state; preserve its identity salt across restores.
    #[arg(long, default_value_os_t = crate::collect::default_state_dir().join("api-pull").join("letta"))]
    pub state: PathBuf,
    #[arg(long, default_value = "stage")]
    pub stage: PathBuf,
    #[arg(long)]
    pub machine: Option<String>,
    /// Serial request pacing in milliseconds.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(0..=60000))]
    pub pace_ms: u64,
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub budget_seconds: u64,
}

pub struct Api {
    client: reqwest::blocking::Client,
    base: String,
    key: Option<String>,
    deadline: Instant,
    pace: Duration,
    next: Instant,
    response_bytes: usize,
}
impl Api {
    pub fn environment(pace: Duration, budget: Duration) -> Result<Self> {
        let key = std::env::var("LETTA_API_KEY")
            .map_err(|_| anyhow::anyhow!("Letta credential unavailable"))?;
        ensure!(!key.trim().is_empty(), "Letta credential unavailable");
        Self::new("https://api.letta.com", Some(key), pace, budget)
    }
    fn new(base: &str, key: Option<String>, pace: Duration, budget: Duration) -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| anyhow::anyhow!("HTTP client unavailable"))?,
            base: base.into(),
            key,
            deadline: Instant::now() + budget,
            pace,
            next: Instant::now(),
            response_bytes: 0,
        })
    }
    fn wait(&self, delay: Duration) -> Result<()> {
        ensure!(
            Instant::now() + delay < self.deadline,
            "Letta pass budget exhausted"
        );
        std::thread::sleep(delay);
        Ok(())
    }
    fn get(&mut self, route: &str, query: &[(&str, String)]) -> Result<Value> {
        for retry in 0..=4 {
            self.wait(self.next.saturating_duration_since(Instant::now()))?;
            let mut request = self
                .client
                .get(format!("{}{route}", self.base))
                .query(query)
                .timeout(
                    self.deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(30)),
                );
            if let Some(key) = &self.key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .map_err(|_| anyhow::anyhow!("Letta request unavailable"))?;
            self.next = Instant::now() + self.pace;
            let status = response.status().as_u16();
            if status == 429 || status == 503 {
                ensure!(retry < 4, "Letta retries exhausted");
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after)
                    .unwrap_or_else(|| {
                        Duration::from_millis(
                            (1 << retry) * 1000 + u64::from(std::process::id() % 251),
                        )
                    });
                self.wait(delay)?;
                continue;
            }
            ensure!(status == 200, "Letta HTTP status {status}");
            let mut body = Vec::new();
            response
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut body)
                .map_err(|_| anyhow::anyhow!("Letta response incomplete"))?;
            ensure!(body.len() <= 16 * 1024 * 1024, "Letta page size exceeded");
            self.response_bytes = self.response_bytes.saturating_add(body.len());
            ensure!(
                self.response_bytes <= 128 * 1024 * 1024,
                "Letta pass byte budget exhausted"
            );
            return serde_json::from_slice(&body)
                .map_err(|_| anyhow::anyhow!("Letta malformed JSON"));
        }
        unreachable!()
    }
    fn pages(&mut self, route: &str, extra: &[(&str, String)], window: bool) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut after = String::new();
        let mut cursors = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut stagnant = 0;
        for _ in 0..10000 {
            let mut query = extra.to_vec();
            query.push(("limit", "100".into()));
            if !after.is_empty() {
                query.push(("after", after.clone()));
            }
            let value = self.get(route, &query)?;
            let page = value.as_array().context("Letta page is not an array")?;
            if page.is_empty() {
                return Ok(rows);
            }
            let old = ids.len();
            for row in page {
                ids.insert(id(row)?.to_owned());
                rows.push(row.clone());
            }
            stagnant = if old == ids.len() { stagnant + 1 } else { 0 };
            if window && stagnant >= 2 {
                return Ok(rows);
            }
            after = id(page.last().context("empty page")?)?.into();
            if !cursors.insert(after.clone()) && !window {
                anyhow::bail!("Letta pagination did not advance");
            }
        }
        anyhow::bail!("Letta page budget exhausted")
    }
}
fn retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(Duration::from_secs(
        (date.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64,
    ))
}
fn safe(value: &str) -> Result<&str> {
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Letta unsafe identifier"
    );
    Ok(value)
}
fn id(value: &Value) -> Result<&str> {
    safe(value["id"].as_str().context("Letta missing identifier")?)
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    sessions: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    high: u64,
    bundles: Vec<String>,
    #[serde(default)]
    required: BTreeSet<String>,
}
#[derive(Default)]
pub struct Report {
    pub agents: usize,
    pub local_only: usize,
    pub bundles: usize,
    pub committed: usize,
    pub incomplete: usize,
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("publication parent missing")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|_| anyhow::anyhow!("publication rename failed"))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn bundle(
    tenant: &str,
    agent: &str,
    session: &str,
    class: &str,
    raw: &str,
    status: Vec<&str>,
    window: bool,
) -> Result<Vec<u8>> {
    let value = json!({"schema":"chat-stasher/inbox@3", "kind":"harness-file", "harness":"letta", "nativeSessionId":session,
        "identity":{"level":"platform_uid","value":tenant}, "capturedAt":chrono::Utc::now().to_rfc3339(),
        "file":{"role":"api-export","relPath":format!("agents/{agent}/conversations/{session}/{class}.jsonl"),"byteStart":0,"byteEnd":raw.len(),"sha256":hash(raw.as_bytes())},
        "raw":{"encoding":"utf-8","data":raw},
        "fidelity": if window { json!({"value":"partial","note":"Agent history is a bounded recent window; local transcripts are a separate source"}) } else { json!({"value":"full","representation":"api"}) },
        "dimensions":{"surface":["cloud"],"tenant":[tenant],"container":[agent],"status":status},
        "producer":{"kind":"collect","version":env!("CARGO_PKG_VERSION"),"platform":"letta"}});
    let bytes = serde_json::to_vec(&value)?;
    crate::inbox::check_bundle(&bytes).map_err(|_| anyhow::anyhow!("Letta bundle invalid"))?;
    Ok(bytes)
}

/// Always replay complete inventories, including below the cursor, to detect
/// edits. Window exhaustion cannot establish absence for agent-only messages.
pub fn run(
    api: &mut Api,
    account: &str,
    local_agents: &Path,
    inbox: &Path,
    state: &Path,
    stage: &Path,
    machine: &str,
    proof: &dyn ArchiveProof,
) -> Result<Report> {
    ensure!(
        !account.trim().is_empty(),
        "Letta account identity unavailable"
    );
    fs::create_dir_all(state)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(state, fs::Permissions::from_mode(0o700))?;
    }
    let pass_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("letta.lock"))?;
    pass_lock
        .try_lock()
        .map_err(|_| anyhow::anyhow!("Letta producer state busy"))?;
    let db = rusqlite::Connection::open(state.join("letta.sqlite3"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            state.join("letta.sqlite3"),
            fs::Permissions::from_mode(0o600),
        )?;
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS identity (salt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS agents (scope TEXT PRIMARY KEY, cursor INTEGER, pending TEXT NOT NULL); BEGIN IMMEDIATE;")?;
    let salt = match db.query_row("SELECT salt FROM identity", [], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(salt) => salt,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            let salt = age::x25519::Identity::generate().to_string();
            let salt = age::secrecy::ExposeSecret::expose_secret(&salt).to_owned();
            db.execute("INSERT INTO identity VALUES (?1)", [&salt])?;
            salt
        }
        Err(error) => return Err(error.into()),
    };
    // Persist the salt before any publication; a crash must not rename sessions.
    db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(salt.as_bytes())
        .map_err(|_| anyhow::anyhow!("identity hash unavailable"))?;
    mac.update(b"chat-stasher/api-pull/letta/account/v1\0");
    mac.update(account.as_bytes());
    let tenant: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let agents = api.pages("/v1/agents/", &[], false)?;
    let conversations = api.pages(
        "/v1/conversations/",
        &[("archive_status", "all".into())],
        false,
    )?;
    let mut known: BTreeSet<String> = agents
        .iter()
        .map(|v| id(v).map(str::to_owned))
        .collect::<Result<_>>()?;
    for conversation in &conversations {
        known.insert(
            safe(
                conversation["agent_id"]
                    .as_str()
                    .context("conversation agent missing")?,
            )?
            .into(),
        );
    }
    let mut report = Report::default();
    match fs::read_dir(local_agents) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().context("local agent name invalid")?;
                if name.starts_with("agent-") {
                    safe(name)?;
                    if known.insert(name.into()) {
                        report.local_only += 1;
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("local agent discovery unavailable"),
    }
    report.agents = known.len();
    for agent in known {
        let scope = format!("{tenant}/{agent}");
        let mut cursor_committed = false;
        let result = (|| -> Result<()> {
            let metadata = api.get(&format!("/v1/agents/{agent}"), &[])?;
            ensure!(id(&metadata)? == agent, "agent metadata identity mismatch");
            let previous: Option<String> = db
                .query_row("SELECT pending FROM agents WHERE scope=?1", [&scope], |r| {
                    r.get(0)
                })
                .optional()?;
            let mut previous: Snapshot = match previous {
                Some(serialized) => serde_json::from_str(&serialized)?,
                None => Snapshot::default(),
            };
            // A prior journal describes an independently completed source
            // pass. Settle it after push, before emitting fresh capture times;
            // otherwise a continuously changing source could never advance.
            if !previous.required.is_empty() {
                crate::test_identity_guard::refuse_fixture_write(&[machine], stage)?;
                fs::create_dir_all(stage)?;
                if prove_receipts(&previous.required, inbox, stage, machine, proof)? {
                    previous.required.clear();
                    db.execute("UPDATE agents SET cursor=MAX(COALESCE(cursor,0),?2),pending=?3 WHERE scope=?1", rusqlite::params![scope, previous.high, serde_json::to_string(&previous)?])?;
                    db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
                    cursor_committed = true;
                }
            }
            let mut snapshot = Snapshot {
                bundles: previous.bundles.clone(),
                required: previous.required.clone(),
                ..Snapshot::default()
            };
            let mut exports = Vec::new();
            let mut capture_order = String::new();
            let mut conversation_ids = BTreeSet::new();
            let mut overlap = BTreeSet::new();
            for conversation in conversations.iter().filter(|c| c["agent_id"] == agent) {
                let session = id(conversation)?;
                ensure!(
                    !session.starts_with("agent-default-"),
                    "reserved default session collision"
                );
                if !conversation_ids.insert(session.to_owned()) {
                    continue;
                }
                let rows =
                    api.pages(&format!("/v1/conversations/{session}/messages"), &[], false)?;
                capture_order.push_str(&observation_order(session, "conversation", &rows)?);
                let (raw, variants, high) = messages(&rows)?;
                overlap.extend(variants.keys().cloned());
                snapshot.high = snapshot.high.max(high);
                snapshot.sessions.insert(session.into(), variants);
                let status = match conversation["archived"].as_bool() {
                    Some(true) => vec!["archived"],
                    Some(false) => vec!["active"],
                    None => vec![],
                };
                exports.push(bundle(
                    &tenant,
                    &agent,
                    session,
                    "messages",
                    &raw,
                    status.clone(),
                    false,
                )?);
                let complete_metadata: String = conversations
                    .iter()
                    .filter(|row| row["id"] == session)
                    .map(|row| crate::import::canonical_json(row) + "\n")
                    .collect();
                exports.push(bundle(
                    &tenant,
                    &agent,
                    session,
                    "conversation-metadata",
                    &complete_metadata,
                    status,
                    false,
                )?);
            }
            let rows = api.pages(&format!("/v1/agents/{agent}/messages"), &[], true)?;
            capture_order.push_str(&observation_order(
                &format!("agent-default-{agent}"),
                "agent-window",
                &rows,
            )?);
            // Same-id different renderings on the agent route belong to their
            // observed conversation too; preserve them instead of discarding.
            let extras: Vec<Value> = rows
                .iter()
                .filter(|r| id(r).is_ok_and(|id| !overlap.contains(id)))
                .cloned()
                .collect();
            let default = format!("agent-default-{agent}");
            let (raw, variants, high) = messages(&extras)?;
            snapshot.high = snapshot.high.max(high);
            snapshot.sessions.insert(default.clone(), variants);
            exports.push(bundle(
                &tenant,
                &agent,
                &default,
                "messages",
                &raw,
                vec![],
                true,
            )?);
            let listing: String = agents
                .iter()
                .filter(|row| row["id"] == agent)
                .map(|row| crate::import::canonical_json(row) + "\n")
                .collect();
            exports.push(bundle(
                &tenant,
                &agent,
                &default,
                "agent-listing",
                &listing,
                vec![],
                true,
            )?);
            exports.push(bundle(
                &tenant,
                &agent,
                &default,
                "agent-metadata",
                &(crate::import::canonical_json(&metadata) + "\n"),
                vec![],
                true,
            )?);
            // Retain the complete window, including overlapping renderings.
            let (window, _, _) = messages(&rows)?;
            exports.push(bundle(
                &tenant,
                &agent,
                &default,
                "agent-window",
                &window,
                vec![],
                true,
            )?);
            exports.push(bundle(
                &tenant,
                &agent,
                &default,
                "capture-order",
                &capture_order,
                vec![],
                true,
            )?);
            let now = chrono::Utc::now().to_rfc3339();
            let mut observations = String::new();
            for (session, old) in &previous.sessions {
                if session.starts_with("agent-default-") {
                    continue;
                }
                for (message, hashes) in old {
                    let current = snapshot.sessions.get(session).and_then(|s| s.get(message));
                    let kind = match current {
                        None if !snapshot.sessions.contains_key(session) => {
                            Some("unavailable at source")
                        }
                        None => Some("absent at source"),
                        Some(new) if !new.is_subset(hashes) => Some("new version"),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        observations.push_str(&serde_json::to_string(&json!({"observation":kind,"account":tenant,"agent":agent,"session":session,"message_id":message,"previous_variants":hashes,"current_variants":current,"observed_at":now,"previous_bundle_digests":previous.bundles,"inventory_sha256":hash(serde_json::to_string(&snapshot.sessions)?.as_bytes()),"scope":"complete conversation inventory; source retention and concurrent mutation remain unknown"}))?);
                        observations.push('\n');
                    }
                }
            }
            if !observations.is_empty() {
                exports.push(bundle(
                    &tenant,
                    &agent,
                    &default,
                    "observations",
                    &observations,
                    vec![],
                    true,
                )?);
            }
            // Keep the cumulative variant set: a temporarily missing rendering
            // must not become a false edit when the server serves it again.
            for (session, current) in &mut snapshot.sessions {
                if let Some(old) = previous.sessions.get(session) {
                    for (message, variants) in current {
                        if let Some(known) = old.get(message) {
                            variants.extend(known.iter().cloned());
                        }
                    }
                }
            }
            crate::test_identity_guard::refuse_fixture_write(&[machine], stage)?;
            fs::create_dir_all(stage)?;
            // Journal durable publication before sealing/proof. The pass lock
            // remains held while SQLite commits, including crash recovery.
            for bytes in &exports {
                let digest = hash(bytes);
                atomic(&inbox.join(format!("letta-{digest}.json")), bytes)?;
                snapshot.required.insert(digest.clone());
                if !snapshot.bundles.contains(&digest) {
                    snapshot.bundles.push(digest);
                }
                report.bundles += 1;
            }
            db.execute("INSERT INTO agents VALUES (?1,NULL,?2) ON CONFLICT(scope) DO UPDATE SET pending=excluded.pending", rusqlite::params![scope, serde_json::to_string(&snapshot)?])?;
            db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            let proven = prove_receipts(&snapshot.required, inbox, stage, machine, proof)?;
            if proven {
                snapshot.required.clear();
                db.execute(
                    "UPDATE agents SET cursor=MAX(COALESCE(cursor,0),?2),pending=?3 WHERE scope=?1",
                    rusqlite::params![scope, snapshot.high, serde_json::to_string(&snapshot)?],
                )?;
            }
            db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            if proven {
                cursor_committed = true;
            } else {
                report.incomplete += 1;
            }
            Ok(())
        })();
        if cursor_committed {
            report.committed += 1;
        }
        if result.is_err() {
            report.incomplete += 1;
        }
    }
    db.execute_batch("COMMIT")?;
    Ok(report)
}
fn prove_receipts(
    required: &BTreeSet<String>,
    inbox: &Path,
    stage: &Path,
    machine: &str,
    proof: &dyn ArchiveProof,
) -> Result<bool> {
    let mut proven = true;
    for digest in required {
        let name = format!("letta-{digest}.json");
        let bytes = fs::read(inbox.join(&name))
            .or_else(|_| fs::read(inbox.join("consumed").join(&name)))
            .context("pending bundle unavailable")?;
        ensure!(hash(&bytes) == *digest, "pending bundle digest mismatch");
        let outcome = crate::inbox::seal_payload(&name, &bytes, stage, machine, 1000, None, None)
            .map_err(|_| anyhow::anyhow!("Letta seal unavailable"))?;
        proven &= proof.holds(machine, &outcome)?;
    }
    Ok(proven)
}

use rusqlite::OptionalExtension;
fn messages(rows: &[Value]) -> Result<(String, BTreeMap<String, BTreeSet<String>>, u64)> {
    let mut ordered = BTreeMap::new();
    let mut variants: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut high = 0;
    for row in rows {
        let id = id(row)?;
        let seq = row["seq_id"]
            .as_u64()
            .context("message sequence unavailable")?;
        let canonical = crate::import::canonical_json(row);
        let digest = hash(canonical.as_bytes());
        high = high.max(seq);
        variants
            .entry(id.into())
            .or_default()
            .insert(digest.clone());
        ordered.insert((seq, id.to_owned(), digest), canonical);
    }
    let raw = ordered.values().map(|s| format!("{s}\n")).collect();
    Ok((raw, variants, high))
}

fn observation_order(session: &str, route: &str, rows: &[Value]) -> Result<String> {
    let mut raw = String::new();
    for (ordinal, row) in rows.iter().enumerate() {
        raw.push_str(&serde_json::to_string(&json!({"session":session,"route":route,"ordinal":ordinal,"message_id":id(row)?,"variant_sha256":hash(crate::import::canonical_json(row).as_bytes())}))?);
        raw.push('\n');
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread,
    };
    fn prepare_stream(stream: &std::net::TcpStream) {
        // Windows accepts inherit the nonblocking listener's mode; a read
        // timeout does not turn them back into blocking connections.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
    }

    #[test]
    fn accepted_connections_wait_for_request_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        // Windows accepts inherit the listener's nonblocking flag. Force that
        // state here so the same regression is exercised on every platform.
        stream.set_nonblocking(true).unwrap();
        prepare_stream(&stream);
        let (started, waiting) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            started.send(()).unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).map(|()| byte)
        });
        waiting.recv().unwrap();
        thread::sleep(Duration::from_millis(50));
        client.write_all(b"G").unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), *b"G");
    }

    struct Server {
        base: String,
        stop: Arc<AtomicBool>,
        mode: Arc<AtomicUsize>,
        thread: Option<thread::JoinHandle<()>>,
        retries: Arc<AtomicUsize>,
    }
    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let mode = Arc::new(AtomicUsize::new(0));
            let retries = Arc::new(AtomicUsize::new(0));
            let (s, m, r) = (stop.clone(), mode.clone(), retries.clone());
            let thread = thread::spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(_) => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                    };
                    prepare_stream(&stream);
                    let mut input = [0; 8192];
                    let count = stream.read(&mut input).unwrap();
                    let request = std::str::from_utf8(&input[..count]).unwrap();
                    // Unit tests deliberately use no credential at all.
                    assert!(!request.to_ascii_lowercase().contains("authorization:"));
                    let path = request.split_whitespace().nth(1).unwrap();
                    let mode = m.load(Ordering::SeqCst);
                    let after = path.contains("after=");
                    let mut status = "200 OK";
                    let mut headers = "";
                    let body = if mode == 3 && path.starts_with("/v1/conversations/") {
                        status = "503 Service Unavailable";
                        json!({})
                    } else if path.starts_with("/v1/agents/agent-visible/messages")
                        && r.fetch_add(1, Ordering::SeqCst) == 0
                    {
                        status = "429 Too Many Requests";
                        headers = "Retry-After: 1\r\n";
                        json!({})
                    } else if path.starts_with("/v1/agents/?") {
                        if after {
                            json!([])
                        } else {
                            json!([{"id":"agent-visible"}])
                        }
                    } else if path.starts_with("/v1/conversations/?") {
                        assert!(path.contains("archive_status=all"));
                        if after {
                            json!([])
                        } else {
                            json!([{"id":"conv-one","agent_id":"agent-visible","archived":true,"is_subagent":true,"unknown":null}])
                        }
                    } else if path.starts_with("/v1/conversations/conv-one/messages") {
                        if (after && path.contains("message-b")) || mode == 2 {
                            json!([])
                        } else if after {
                            json!([{"id":"message-a","seq_id":1,"content":"synthetic","render":true},{"id":"message-b","seq_id":2,"content":"synthetic"}])
                        } else {
                            json!([{"id":"message-a","seq_id":1,"content":if mode==1 {"synthetic edit"} else {"synthetic"},"unknown":{"null":null}}])
                        }
                    } else if path.ends_with("/messages?limit=100")
                        || path.contains("/messages?limit=100&after=")
                    {
                        if after {
                            json!([])
                        } else {
                            json!([{"id":"message-window","seq_id":3,"tool_returns":[{"unknown":null}]}])
                        }
                    } else if path.starts_with("/v1/agents/agent-") {
                        json!({"id":if path.contains("agent-hidden") {"agent-hidden"} else {"agent-visible"},"hidden":path.contains("agent-hidden"),"unknown":null})
                    } else {
                        panic!("unexpected mock route");
                    };
                    let body = serde_json::to_vec(&body).unwrap();
                    write!(stream,"HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                    stream.write_all(&body).unwrap();
                }
            });
            Self {
                base,
                stop,
                mode,
                thread: Some(thread),
                retries,
            }
        }
        fn api(&self) -> Api {
            Api::new(&self.base, None, Duration::ZERO, Duration::from_secs(20)).unwrap()
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    struct Proof(bool);
    impl ArchiveProof for Proof {
        fn holds(&self, _: &str, _: &crate::inbox::SealOutcome) -> Result<bool> {
            Ok(self.0)
        }
    }
    fn captured(inbox: &Path) -> Vec<Value> {
        fs::read_dir(inbox)
            .unwrap()
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect()
    }
    #[test]
    fn mock_vertical_slice_versions_absence_retry_and_hidden_agent() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let server = Server::start();
        let start = Instant::now();
        let pull = |proof: bool| {
            run(
                &mut server.api(),
                "synthetic-account",
                &local,
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(proof),
            )
            .unwrap()
        };
        let first = pull(false);
        assert_eq!(first.agents, 2);
        assert_eq!(first.local_only, 1);
        assert_eq!(first.bundles, 12);
        assert_eq!(first.committed, 0);
        assert_eq!(first.incomplete, 2);
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert!(server.retries.load(Ordering::SeqCst) >= 2);
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM agents WHERE cursor IS NOT NULL",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        let exports = captured(&root.join("inbox"));
        let messages = exports
            .iter()
            .find(|v| {
                v["nativeSessionId"] == "conv-one"
                    && v["file"]["relPath"]
                        .as_str()
                        .unwrap()
                        .ends_with("/messages.jsonl")
            })
            .unwrap();
        assert_eq!(messages["file"]["role"], "api-export");
        assert_eq!(messages["fidelity"]["representation"], "api");
        assert_eq!(messages["dimensions"]["status"], json!(["archived"]));
        assert_eq!(
            messages["dimensions"]["tenant"][0].as_str().unwrap().len(),
            64
        );
        let rows: Vec<Value> = messages["raw"]["data"]
            .as_str()
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["seq_id"], 1);
        assert_eq!(rows[2]["seq_id"], 2);
        assert!(rows.iter().any(|r| r.get("unknown").is_some()));
        let repeat = pull(true);
        assert_eq!(repeat.committed, 2);
        assert!(!captured(&root.join("inbox"))
            .iter()
            .any(|v| v["raw"]["data"].as_str().unwrap().contains("new version")));
        server.mode.store(1, Ordering::SeqCst);
        assert_eq!(pull(true).committed, 2);
        let old_files = fs::read_dir(root.join("inbox")).unwrap().count();
        assert!(captured(&root.join("inbox"))
            .iter()
            .any(|v| v["raw"]["data"].as_str().unwrap().contains("new version")));
        let edits = captured(&root.join("inbox"))
            .iter()
            .filter(|v| v["raw"]["data"].as_str().unwrap().contains("new version"))
            .count();
        server.mode.store(0, Ordering::SeqCst);
        assert_eq!(pull(true).committed, 2);
        assert_eq!(
            captured(&root.join("inbox"))
                .iter()
                .filter(|v| v["raw"]["data"].as_str().unwrap().contains("new version"))
                .count(),
            edits
        );
        server.mode.store(2, Ordering::SeqCst);
        assert_eq!(pull(true).committed, 2);
        assert!(fs::read_dir(root.join("inbox")).unwrap().count() > old_files);
        assert!(captured(&root.join("inbox"))
            .iter()
            .any(|v| v["raw"]["data"]
                .as_str()
                .unwrap()
                .contains("absent at source")));
        // Every published export is sealed by the existing sink, not another writer.
        assert!(crate::store::session_shard_dir(
            &root.join("stage"),
            "synthetic-machine",
            &format!(
                "letta.{}.conv-one",
                messages["dimensions"]["tenant"][0].as_str().unwrap()
            )
        )
        .exists());
    }
    #[test]
    fn partial_enumeration_never_commits_or_establishes_absence() {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start();
        server.mode.store(3, Ordering::SeqCst);
        let result = run(
            &mut Api::new(
                &server.base,
                None,
                Duration::ZERO,
                Duration::from_millis(200),
            )
            .unwrap(),
            "synthetic-account",
            &root.path().join("agents"),
            &root.path().join("inbox"),
            &root.path().join("state"),
            &root.path().join("stage"),
            "synthetic-machine",
            &Proof(true),
        );
        assert!(result.is_err());
        assert!(!root.path().join("inbox").exists());
        let db = rusqlite::Connection::open(root.path().join("state/letta.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agents", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn previously_archived_receipts_commit_before_fresh_unarchived_captures() {
        struct Held(BTreeSet<String>);
        impl ArchiveProof for Held {
            fn holds(&self, _: &str, outcome: &crate::inbox::SealOutcome) -> Result<bool> {
                let digest = match outcome {
                    crate::inbox::SealOutcome::Stored(row) => &row.file_sha256,
                    crate::inbox::SealOutcome::Duplicate(row) => &row.file_sha256,
                };
                Ok(self.0.contains(digest))
            }
        }
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let server = Server::start();
        let pull = |proof: &dyn ArchiveProof| {
            run(
                &mut server.api(),
                "synthetic-account",
                &local,
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                proof,
            )
            .unwrap()
        };
        assert_eq!(pull(&Proof(false)).committed, 0);
        let held = Held(
            fs::read_dir(root.join("inbox"))
                .unwrap()
                .map(|entry| hash(&fs::read(entry.unwrap().path()).unwrap()))
                .collect(),
        );
        let fresh = pull(&held);
        assert_eq!(fresh.committed, 2);
        assert_eq!(fresh.incomplete, 2);
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agents WHERE cursor=3", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            2
        );
    }

    #[test]
    fn lost_pending_bundle_cannot_be_replaced_by_a_new_pass_receipt() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let pull = |held| {
            run(
                &mut server.api(),
                "synthetic-account",
                &local,
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(held),
            )
            .unwrap()
        };
        assert_eq!(pull(false).committed, 0);
        let missing = fs::read_dir(root.join("inbox"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                value["nativeSessionId"] == "conv-one"
                    && value["file"]["relPath"]
                        .as_str()
                        .unwrap()
                        .ends_with("/messages.jsonl")
            })
            .unwrap();
        fs::remove_file(missing).unwrap();
        let replay = pull(true);
        assert_eq!(replay.committed, 1);
        assert_eq!(replay.incomplete, 1);
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        let cursor: Option<u64> = db
            .query_row(
                "SELECT cursor FROM agents WHERE scope LIKE '%/agent-visible'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor, None);
    }

    #[test]
    fn sequences_identifiers_and_retry_dates_are_checked() {
        assert!(messages(&[json!({"id":"message-a"})]).is_err());
        assert!(safe("../escape").is_err());
        let precise: Value = serde_json::from_str(
            r#"{"id":"message-precise","seq_id":1,"unknown":123456789.1234567890123456789}"#,
        )
        .unwrap();
        assert!(messages(&[precise])
            .unwrap()
            .0
            .contains("123456789.1234567890123456789"));
        assert_eq!(retry_after("2"), Some(Duration::from_secs(2)));
        assert!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT").is_some());
        assert!(retry_after("invalid").is_none());
    }
}

#[cfg(test)]
mod round_trip_tests {
    use super::*;
    #[test]
    fn api_export_fixtures_seal_and_read_back_without_field_loss() {
        let sandbox = crate::test_support::Sandbox::new();
        let stage = sandbox.root().join("stage");
        fs::create_dir_all(&stage).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox");
        for name in [
            "api-export-utf8.json",
            "api-export-base64.json",
            "api-export-parent.json",
        ] {
            let bytes = fs::read(root.join(name)).unwrap();
            let input: Value = serde_json::from_slice(&bytes).unwrap();
            let outcome = crate::inbox::seal_payload(
                name,
                &bytes,
                &stage,
                "synthetic-machine",
                1000,
                None,
                None,
            )
            .unwrap();
            let crate::inbox::SealOutcome::Stored(row) = outcome else {
                panic!("new fixture must be stored")
            };
            let dir = crate::store::session_shard_dir(&stage, "synthetic-machine", &row.id);
            let path = crate::store::sealed_shard_entries(&dir)
                .unwrap()
                .into_iter()
                .find(|(_, p)| p.file_name().unwrap() == row.shard.as_str())
                .unwrap()
                .1;
            let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(record["raw"], input["raw"]);
            assert_eq!(record["file"], input["file"]);
            assert_eq!(record["dimensions"], input["dimensions"]);
            assert_eq!(record["fidelity"], input["fidelity"]);
            assert_eq!(record["producer"], input["producer"]);
        }
    }
}
