//! Bounded Letta API producer. Local inbox publication and archive proof are
//! separate: only destination proof settles the per-account, per-agent pass.
//!
//! Two entry points share one pipeline: the `pull letta` command (its own
//! account, inbox and pacing on the command line) and the `[pull.letta]`
//! declaration the scheduled pass pulls before collect (`[PassPull]`). The
//! pass declaration resolves through [`pass_pull`] and the command through
//! [`PullArgs`], and both feed the same [`run`] with the same pacing, page
//! budget and `Retry-After` handling.
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

/// Serial request pacing default: about one request per second.
pub const DEFAULT_PACE_MS: u64 = 1000;
/// Largest accepted serial pacing interval, in milliseconds.
pub const MAX_PACE_MS: u64 = 60_000;
/// Default whole-pass wall-clock budget, in seconds.
pub const DEFAULT_BUDGET_SECONDS: u64 = 600;
/// Largest accepted whole-pass budget: one hour.
pub const MAX_BUDGET_SECONDS: u64 = 3_600;
/// Rows requested per page. The provider rejects larger agent-history pages,
/// so the accepted band is 100–200 rows and the default sits inside it.
pub const DEFAULT_PAGE_SIZE: u64 = 100;
/// Smallest page this producer will request.
pub const MIN_PAGE_SIZE: u64 = 100;
/// Largest page this producer will request; the provider's own cap (larger
/// agent-history pages were measured to be rate-limit-refused at the source).
pub const MAX_PAGE_SIZE: u64 = 200;

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
    /// Serial request pacing in milliseconds (default ~1 request/second).
    #[arg(long, default_value_t = DEFAULT_PACE_MS, value_parser = clap::value_parser!(u64).range(0..=MAX_PACE_MS))]
    pub pace_ms: u64,
    /// Whole-pass wall-clock budget in seconds.
    #[arg(long, default_value_t = DEFAULT_BUDGET_SECONDS, value_parser = clap::value_parser!(u64).range(1..=MAX_BUDGET_SECONDS))]
    pub budget_seconds: u64,
    /// Rows requested per page. The provider caps agent-history pages, so the
    /// accepted band is 100–200 rows and the default sits inside it.
    #[arg(long, default_value_t = DEFAULT_PAGE_SIZE, value_parser = clap::value_parser!(u64).range(MIN_PAGE_SIZE..=MAX_PAGE_SIZE))]
    pub page_size: u64,
}

/// The `[pull.letta]` declaration as a scheduled pass uses it: validated,
/// with defaults applied once, and the producer state defaulted beside the
/// pass's own state. `None` (from [`pass_pull`]) means no producer declared.
pub struct PassPull {
    /// Stable provider account ID. Never logged.
    pub account: String,
    /// Local inbox for ordinary @3 harness-file bundles.
    pub inbox: PathBuf,
    /// Durable private producer state.
    pub state: PathBuf,
    /// Serial request pacing in milliseconds (default ~1 request/second).
    pub pace_ms: u64,
    /// Whole-pass wall-clock budget in seconds.
    pub budget_seconds: u64,
    /// Rows requested per page (`MIN_PAGE_SIZE..=MAX_PAGE_SIZE`).
    pub page_size: u64,
}

/// Resolve the declared `[pull.letta]` producer for a scheduled pass.
///
/// An absent section is `Ok(None)`: declaring the producer is opt-in, so an
/// unchanged config keeps the pass's behaviour unchanged. A section that is
/// present but cannot name the account or the inbox, or asks for pacing,
/// budget or page size outside the accepted band, is an error naming the key
/// it faults — a declared producer whose declaration silently never ran would
/// look exactly like an archived one, which is the failure this refuses.
pub fn pass_pull(config: &crate::config::Config) -> Result<Option<PassPull>> {
    let Some(pull) = config.pull.as_ref().and_then(|pull| pull.letta.as_ref()) else {
        return Ok(None);
    };
    let account = pull
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context(
            "pull.letta.account_id is missing: the declared producer cannot name the account it reads",
        )?
        .to_owned();
    let inbox = pull
        .inbox
        .as_deref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .context(
            "pull.letta.inbox is missing: the declared producer publishes inbox@3 bundles into \
             that directory, so declaring it means naming the inbox",
        )?;
    let pace_ms = pull.pace_ms.unwrap_or(DEFAULT_PACE_MS);
    ensure!(
        pace_ms <= MAX_PACE_MS,
        "pull.letta.pace_ms = {pace_ms} is outside the accepted 0–{MAX_PACE_MS} millisecond band"
    );
    let budget_seconds = pull.budget_seconds.unwrap_or(DEFAULT_BUDGET_SECONDS);
    ensure!(
        (1..=MAX_BUDGET_SECONDS).contains(&budget_seconds),
        "pull.letta.budget_seconds = {budget_seconds} is outside the accepted \
         1–{MAX_BUDGET_SECONDS} second band"
    );
    let page_size = pull.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    ensure!(
        (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size),
        "pull.letta.page_size = {page_size} is outside the accepted \
         {MIN_PAGE_SIZE}–{MAX_PAGE_SIZE} row band the provider serves"
    );
    let state = pull
        .state
        .as_deref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(value.trim()))
        .unwrap_or_else(|| {
            crate::collect::default_state_dir()
                .join("api-pull")
                .join("letta")
        });
    Ok(Some(PassPull {
        account,
        inbox: PathBuf::from(inbox),
        state,
        pace_ms,
        budget_seconds,
        page_size,
    }))
}

pub struct Api {
    client: reqwest::blocking::Client,
    base: String,
    key: Option<String>,
    deadline: Instant,
    pace: Duration,
    next: Instant,
    response_bytes: usize,
    page_size: u64,
}
impl Api {
    pub fn environment(pace: Duration, budget: Duration, page_size: u64) -> Result<Self> {
        let key = std::env::var("LETTA_API_KEY")
            .map_err(|_| anyhow::anyhow!("Letta credential unavailable"))?;
        ensure!(!key.trim().is_empty(), "Letta credential unavailable");
        Self::new("https://api.letta.com", Some(key), pace, budget, page_size)
    }
    fn new(
        base: &str,
        key: Option<String>,
        pace: Duration,
        budget: Duration,
        page_size: u64,
    ) -> Result<Self> {
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
            page_size,
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
        self.request(route, query, false)?
            .context("Letta response unavailable")
    }
    fn organization(&mut self) -> Result<Option<String>> {
        // This route lists connections for the authenticated organization.
        // An unsupported/forbidden route or an empty list cannot identify it.
        let Some(value) = self.request("/v1/environments", &[], true)? else {
            return Ok(None);
        };
        let connections = value["connections"]
            .as_array()
            .context("Letta organization response invalid")?;
        let mut organizations = BTreeSet::new();
        for connection in connections {
            let organization = connection["organizationId"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .context("Letta organization identity unavailable")?;
            organizations.insert(organization.to_owned());
        }
        ensure!(
            organizations.len() <= 1,
            "Letta organization identity ambiguous"
        );
        Ok(organizations.into_iter().next())
    }
    fn request(
        &mut self,
        route: &str,
        query: &[(&str, String)],
        optional_identity: bool,
    ) -> Result<Option<Value>> {
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
            if optional_identity && (status == 403 || status == 404) {
                return Ok(None);
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
                .map(Some)
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
            query.push(("limit", self.page_size.to_string()));
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
fn scoped_identity(salt: &str, domain: &[u8], value: &str) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(salt.as_bytes())
        .map_err(|_| anyhow::anyhow!("identity hash unavailable"))?;
    mac.update(domain);
    mac.update(value.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    sessions: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
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
    atomic_write(path, |temp| {
        temp.write_all(bytes)?;
        Ok(())
    })
}
fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut tempfile::NamedTempFile) -> Result<()>,
) -> Result<()> {
    let parent = path.parent().context("publication parent missing")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::Builder::new()
        .prefix("letta-")
        .suffix(".part")
        .tempfile_in(parent)?;
    write(&mut temp)?;
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

/// Always replay complete inventories to detect edits at earlier sequences.
/// Window exhaustion cannot establish absence for agent-only messages.
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
    // Retain the legacy cursor column for state compatibility, but do not
    // record a sequence checkpoint: edit/absence detection requires full replay.
    db.execute_batch("CREATE TABLE IF NOT EXISTS identity (salt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS agents (scope TEXT PRIMARY KEY, cursor INTEGER, pending TEXT NOT NULL); CREATE TABLE IF NOT EXISTS account_bindings (organization TEXT PRIMARY KEY, tenant TEXT NOT NULL UNIQUE); BEGIN IMMEDIATE; UPDATE agents SET cursor=NULL WHERE cursor IS NOT NULL;")?;
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
    let tenant = scoped_identity(&salt, b"chat-stasher/api-pull/letta/account/v1\0", account)?;
    if let Some(organization) = api.organization()? {
        // Bind an observed organization to the existing caller-derived tenant,
        // without assuming an organization ID is a literal user account ID.
        let organization = scoped_identity(
            &salt,
            b"chat-stasher/api-pull/letta/organization/v1\0",
            &organization,
        )?;
        let conflicts: u64 = db.query_row(
            "SELECT COUNT(*) FROM account_bindings WHERE (organization=?1 AND tenant<>?2) OR (tenant=?2 AND organization<>?1)",
            rusqlite::params![organization, tenant], |row| row.get(0),
        )?;
        ensure!(conflicts == 0, "Letta observed account scope mismatch");
        db.execute(
            "INSERT OR IGNORE INTO account_bindings VALUES (?1,?2)",
            rusqlite::params![organization, tenant],
        )?;
        // Identity binding, like the salt, survives a crash before publication.
        db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
    }
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
        let mut pass_proven = false;
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
                    previous.bundles = previous.required.iter().cloned().collect();
                    previous.required.clear();
                    db.execute(
                        "UPDATE agents SET pending=?2 WHERE scope=?1",
                        rusqlite::params![scope, serde_json::to_string(&previous)?],
                    )?;
                    db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
                    pass_proven = true;
                } else {
                    // Keep exactly this completed pass until its receipts are
                    // proven. Fresh timestamps would otherwise grow the journal
                    // without bound while archive proof remains unavailable.
                    report.incomplete += 1;
                    return Ok(());
                }
            }
            let mut snapshot = Snapshot::default();
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
                let (raw, variants) = messages(&rows)?;
                overlap.extend(variants.keys().cloned());
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
            let (raw, variants) = messages(&extras)?;
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
            let (window, _) = messages(&rows)?;
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
                    "UPDATE agents SET pending=?2 WHERE scope=?1",
                    rusqlite::params![scope, serde_json::to_string(&snapshot)?],
                )?;
            }
            db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            if proven {
                pass_proven = true;
            } else {
                report.incomplete += 1;
            }
            Ok(())
        })();
        if pass_proven {
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
fn messages(rows: &[Value]) -> Result<(String, BTreeMap<String, BTreeSet<String>>)> {
    let mut ordered = BTreeMap::new();
    let mut variants: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let id = id(row)?;
        let seq = row["seq_id"]
            .as_u64()
            .context("message sequence unavailable")?;
        let canonical = crate::import::canonical_json(row);
        let digest = hash(canonical.as_bytes());
        variants
            .entry(id.into())
            .or_default()
            .insert(digest.clone());
        ordered.insert((seq, id.to_owned(), digest), canonical);
    }
    let raw = ordered.values().map(|s| format!("{s}\n")).collect();
    Ok((raw, variants))
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
    fn publication_is_invisible_until_rename_and_crash_residue_is_ignored() {
        use crate::bundle_transport::{BundleTransport, LocalFolder};
        let sandbox = crate::test_support::Sandbox::new();
        let inbox = sandbox.root().join("inbox");
        let folder = LocalFolder::new(&inbox).unwrap();
        let published = inbox.join("letta-synthetic.json");
        atomic_write(&published, |temp| {
            temp.write_all(b"{")?;
            let listing = folder.list()?;
            assert_eq!(listing.total_inbox_files, 0);
            assert!(listing.items.is_empty());
            assert_eq!(listing.part_files_seen, 1);
            // Preserve a truncated copy under the actual publication name to
            // model a crash leaving the temp entry behind.
            fs::copy(
                temp.path(),
                inbox.join(format!(
                    "crash-{}",
                    temp.path().file_name().unwrap().to_str().unwrap()
                )),
            )?;
            temp.write_all(b"}")?;
            Ok(())
        })
        .unwrap();
        let listing = folder.list().unwrap();
        assert_eq!(listing.total_inbox_files, 1);
        assert_eq!(listing.part_files_seen, 1);
        assert_eq!(listing.items[0].item, published);
        assert_eq!(fs::read(&published).unwrap(), b"{}");
    }

    #[test]
    fn full_replay_snapshot_has_no_unused_high_water_mark() {
        let value = serde_json::to_value(Snapshot::default()).unwrap();
        assert!(value.get("high").is_none());
    }

    #[test]
    fn receipt_journal_stays_bounded_across_proven_and_unproven_passes() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let pull = |held| {
            run(
                &mut server.api(),
                "synthetic-account",
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(held),
            )
            .unwrap()
        };
        let pending = || {
            let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
            let serialized: String = db
                .query_row("SELECT pending FROM agents", [], |r| r.get(0))
                .unwrap();
            serde_json::from_str::<Value>(&serialized).unwrap()
        };
        for _ in 0..3 {
            let report = pull(true);
            assert_eq!(report.committed, 1);
            let snapshot = pending();
            assert_eq!(
                snapshot["bundles"].as_array().unwrap().len(),
                report.bundles
            );
        }
        assert_eq!(pull(false).incomplete, 1);
        let before = pending();
        for _ in 0..3 {
            let report = pull(false);
            assert_eq!(report.incomplete, 1);
            assert_eq!(
                report.bundles, 0,
                "unproven receipts block fresh publication for this agent"
            );
            assert_eq!(pending(), before);
        }
        assert_eq!(pull(true).committed, 1);
    }

    #[test]
    fn unproven_pass_does_not_accumulate_fresh_receipts() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let pull = || {
            run(
                &mut server.api(),
                "synthetic-account",
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(false),
            )
            .unwrap()
        };
        assert_eq!(pull().incomplete, 1);
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        let before: String = db
            .query_row("SELECT pending FROM agents", [], |r| r.get(0))
            .unwrap();
        let replay = pull();
        assert_eq!(replay.incomplete, 1);
        assert_eq!(replay.bundles, 0);
        let after: String = db
            .query_row("SELECT pending FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, before);
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
        requests: Arc<AtomicUsize>,
        /// 429 answers the mode-12 listing arm actually sent. The shared
        /// `retries` counter also counts ordinary attempts of the visible
        /// agent's 429 arm, so a test that wants "the rate-limited page was
        /// answered 429 exactly once and retried" counts here.
        listing_429s: Arc<AtomicUsize>,
        limits: Arc<std::sync::Mutex<Vec<u64>>>,
    }
    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let mode = Arc::new(AtomicUsize::new(0));
            let retries = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));
            let listing_429s = Arc::new(AtomicUsize::new(0));
            let limits = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (s, m, r, q, g, l) = (
                stop.clone(),
                mode.clone(),
                retries.clone(),
                requests.clone(),
                listing_429s.clone(),
                limits.clone(),
            );
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
                    q.fetch_add(1, Ordering::SeqCst);
                    // Every paged listing or message walk carries a `limit`
                    // parameter; the value is recorded so a test can prove the
                    // configured page size reached the wire.
                    let limit = path
                        .split("limit=")
                        .nth(1)
                        .and_then(|rest| rest.split('&').next())
                        .and_then(|value| value.parse::<u64>().ok());
                    if let Some(page) = limit {
                        ensure_page_size(path, page);
                        l.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(page);
                    }
                    let mode = m.load(Ordering::SeqCst);
                    let after = path.contains("after=");
                    let mut status = "200 OK";
                    let mut headers = "";
                    let body = if path.starts_with("/v1/environments") {
                        if mode == 8 || mode == 9 {
                            status = if mode == 8 {
                                "404 Not Found"
                            } else {
                                "403 Forbidden"
                            };
                            json!({})
                        } else if mode == 10 {
                            json!({"connections":[{"organizationId":"synthetic-organization-a"},{"organizationId":"synthetic-organization-b"}],"hasNextPage":false})
                        } else if mode == 6 || mode == 7 {
                            json!({"connections":[{"id":"synthetic-connection", "organizationId":if mode == 6 {"synthetic-organization-a"} else {"synthetic-organization-b"}}],"hasNextPage":false})
                        } else {
                            json!({"connections":[],"hasNextPage":false})
                        }
                    } else if mode == 3 && path.starts_with("/v1/conversations/") {
                        status = "503 Service Unavailable";
                        json!({})
                    } else if mode == 11 && path.starts_with("/v1/agents/agent-hidden/messages") {
                        // A source that cannot finish serving an agent's
                        // history: retried, then exhausted mid-pass.
                        status = "503 Service Unavailable";
                        headers = "Retry-After: 0\r\n";
                        json!({})
                    } else if mode == 12
                        && path.starts_with("/v1/agents/?")
                        && g.swap(1, Ordering::SeqCst) == 0
                    {
                        status = "429 Too Many Requests";
                        headers = "Retry-After: 1\r\n";
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
                    } else if limit.is_some() && path.contains("/messages?") {
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
                requests,
                listing_429s,
                limits,
            }
        }
        fn api(&self) -> Api {
            Api::new(
                &self.base,
                None,
                Duration::ZERO,
                Duration::from_secs(20),
                DEFAULT_PAGE_SIZE,
            )
            .unwrap()
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
    fn observable_organization_refuses_tenant_forks_and_account_changes() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        server.mode.store(6, Ordering::SeqCst);
        let pull = |account| {
            run(
                &mut server.api(),
                account,
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
        };
        assert_eq!(pull("synthetic-account").unwrap().committed, 1);
        let files = fs::read_dir(root.join("inbox")).unwrap().count();
        assert!(
            pull("synthetic-account-typo").is_err(),
            "an observed organization cannot silently fork tenant scope"
        );
        assert_eq!(fs::read_dir(root.join("inbox")).unwrap().count(), files);
        server.mode.store(7, Ordering::SeqCst);
        assert!(
            pull("synthetic-account").is_err(),
            "a supplied account cannot silently switch observed organization"
        );
        assert_eq!(fs::read_dir(root.join("inbox")).unwrap().count(), files);
        server.mode.store(6, Ordering::SeqCst);
        assert_eq!(pull("synthetic-account").unwrap().committed, 1);
        let files = fs::read_dir(root.join("inbox")).unwrap().count();
        server.mode.store(10, Ordering::SeqCst);
        assert!(pull("synthetic-account").is_err());
        assert_eq!(fs::read_dir(root.join("inbox")).unwrap().count(), files);
        let state = fs::read(root.join("state/letta.sqlite3")).unwrap();
        for secret in ["synthetic-account", "synthetic-organization-a"] {
            assert!(!state
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes()));
        }
    }

    #[test]
    fn unavailable_organization_probe_keeps_the_documented_unverified_scope() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        for mode in [8, 9] {
            server.mode.store(mode, Ordering::SeqCst);
            let report = run(
                &mut server.api(),
                "synthetic-account",
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
            .unwrap();
            assert_eq!(report.committed, 1);
            assert_eq!(report.incomplete, 0);
        }
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
                DEFAULT_PAGE_SIZE,
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
    fn previously_archived_receipts_are_proven_before_fresh_unarchived_captures() {
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
            db.query_row("SELECT COUNT(*) FROM agents WHERE cursor IS NULL AND json_array_length(json_extract(pending, '$.required')) > 0", [], |row| row
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

    /// Every paged request in these tests must stay inside the band the
    /// producer is allowed to ask for; a wider or narrower page reaching the
    /// wire would otherwise pass unnoticed.
    fn ensure_page_size(path: &str, page: u64) {
        assert!(
            (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page),
            "page size {page} reached the wire outside the accepted \
             {MIN_PAGE_SIZE}–{MAX_PAGE_SIZE} row band: {path}"
        );
    }

    #[test]
    fn pass_pull_resolves_the_declaration_or_names_what_it_lacks() {
        let declared =
            |account: Option<&str>, inbox: Option<&str>, page: Option<u64>| crate::config::Config {
                pull: Some(crate::config::PullSectionConfig {
                    letta: Some(crate::config::LettaPullConfig {
                        account_id: account.map(str::to_owned),
                        inbox: inbox.map(str::to_owned),
                        state: None,
                        pace_ms: None,
                        budget_seconds: None,
                        page_size: page,
                    }),
                }),
                ..Default::default()
            };
        // Absent section and empty `[pull]` table are both "not declared".
        assert_eq!(
            pass_pull(&crate::config::Config::default())
                .unwrap()
                .is_none(),
            true
        );
        assert_eq!(
            pass_pull(&crate::config::Config {
                pull: Some(crate::config::PullSectionConfig::default()),
                ..Default::default()
            })
            .unwrap()
            .is_none(),
            true
        );
        // A valid resolution applies the documented defaults exactly once;
        // the producer state defaults beside the pass's own state.
        let resolved = pass_pull(&declared(
            Some("synthetic-account"),
            Some("/inbox"),
            Some(150),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(resolved.account, "synthetic-account");
        assert_eq!(resolved.inbox, PathBuf::from("/inbox"));
        assert_eq!(resolved.page_size, 150);
        assert_eq!(resolved.pace_ms, DEFAULT_PACE_MS);
        assert_eq!(resolved.budget_seconds, DEFAULT_BUDGET_SECONDS);
        assert_eq!(
            resolved.state,
            crate::collect::default_state_dir()
                .join("api-pull")
                .join("letta")
        );
        assert_eq!(
            pass_pull(&declared(Some(" synthetic-account "), Some("/inbox"), None))
                .unwrap()
                .unwrap()
                .account,
            "synthetic-account"
        );
        // A declaration that cannot run names the key at fault rather than
        // silently degrading to defaults the user never wrote.
        let error = |config: crate::config::Config| match pass_pull(&config) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a declaration that cannot run must be refused"),
        };
        let missing_account = error(declared(Some("   "), Some("/inbox"), Some(100)));
        assert!(
            missing_account.contains("pull.letta.account_id"),
            "missing account must name its key: {missing_account}"
        );
        let missing_inbox = error(declared(Some("synthetic-account"), None, Some(100)));
        assert!(
            missing_inbox.contains("pull.letta.inbox"),
            "missing inbox must name its key: {missing_inbox}"
        );
        for page in [Some(99u64), Some(201u64)] {
            let outside = error(declared(Some("synthetic-account"), Some("/inbox"), page));
            assert!(
                outside.contains("pull.letta.page_size"),
                "out-of-band page size must name its key: {outside}"
            );
        }
        let mut overpaced = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
        overpaced
            .pull
            .as_mut()
            .unwrap()
            .letta
            .as_mut()
            .unwrap()
            .pace_ms = Some(MAX_PACE_MS + 1);
        assert!(error(overpaced).contains("pull.letta.pace_ms"));
        let mut unbudgeted = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
        unbudgeted
            .pull
            .as_mut()
            .unwrap()
            .letta
            .as_mut()
            .unwrap()
            .budget_seconds = Some(0);
        assert!(error(unbudgeted).contains("pull.letta.budget_seconds"));
        // An explicit state is honoured verbatim.
        let mut restated = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
        restated
            .pull
            .as_mut()
            .unwrap()
            .letta
            .as_mut()
            .unwrap()
            .state = Some("/elsewhere/letta".to_owned());
        assert_eq!(
            pass_pull(&restated).unwrap().unwrap().state,
            PathBuf::from("/elsewhere/letta")
        );
    }

    #[test]
    fn pass_uses_the_configured_page_size_on_every_serial_paced_request() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let pace = Duration::from_millis(100);
        let start = Instant::now();
        let mut api = Api::new(&server.base, None, pace, Duration::from_secs(60), 150).unwrap();
        let report = run(
            &mut api,
            "synthetic-account",
            &root.join("agents"),
            &root.join("inbox"),
            &root.join("state"),
            &root.join("stage"),
            "synthetic-machine",
            &Proof(true),
        )
        .unwrap();
        assert_eq!(report.incomplete, 0, "the paced pass itself must settle");
        let observed = server
            .limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            !observed.is_empty(),
            "the walk must request at least one paged listing"
        );
        assert!(
            observed.iter().all(|page| *page == 150),
            "every request must carry the configured page size, saw {observed:?}"
        );
        let requests = server.requests.load(Ordering::SeqCst) as u32;
        assert!(requests >= 10, "a full walk is at least ten requests");
        // Serial pacing: request n cannot start before one full pace has
        // passed since request n-1's response, so the whole pass takes at
        // least (n-1) paces. This fails if the pacing clock is ignored even
        // though every response here is an immediate 200-family answer.
        assert!(
            start.elapsed() >= pace * (requests - 1),
            "the pass must take at least one pace per request after the first: \
             {requests} requests in {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn rate_limited_page_is_waited_out_and_retried_inside_a_pass() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        server.mode.store(12, Ordering::SeqCst);
        let start = Instant::now();
        let mut api = server.api();
        let report = run(
            &mut api,
            "synthetic-account",
            &root.join("agents"),
            &root.join("inbox"),
            &root.join("state"),
            &root.join("stage"),
            "synthetic-machine",
            &Proof(true),
        )
        .unwrap();
        assert_eq!(report.agents, 1, "the retried listing served its rows");
        assert_eq!(
            report.incomplete, 0,
            "a Retry-After waited out is a complete pass, not a refusal"
        );
        assert_eq!(
            report.committed, 1,
            "the whole pass is proven after the retry"
        );
        assert_eq!(
            server.listing_429s.load(Ordering::SeqCst),
            1,
            "exactly the rate-limited page was answered 429, once"
        );
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "the Retry-After delay must actually be waited out: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn mid_pass_source_failure_keeps_the_remainder_pending_not_empty() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let pull = |api: &mut Api, proof: &dyn ArchiveProof| {
            run(
                api,
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
        // A complete, proven pass first, so the later failure has a prior
        // journal to leave intact.
        let first = pull(&mut server.api(), &Proof(true));
        assert_eq!(first.agents, 2);
        assert_eq!(first.incomplete, 0);
        let pending_of = |agent: &str| {
            let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
            db.query_row(
                "SELECT pending FROM agents WHERE scope LIKE ?",
                rusqlite::params![format!("%/{agent}")],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let before = (pending_of("agent-hidden"), pending_of("agent-visible"));
        // Mid-pass failure: the hidden agent's history cannot be served.
        // `Retry-After: 0` makes the bounded retries exhaust immediately, so
        // the failure is contained in the report instead of eating the whole
        // pass budget the remaining agent still needs.
        server.mode.store(11, Ordering::SeqCst);
        let mut api = server.api();
        let second = pull(&mut api, &Proof(true));
        assert_eq!(
            second.incomplete, 1,
            "the unread agent is a halted remainder, not an absent one"
        );
        assert_eq!(second.committed, 1, "the other agent's pass still settles");
        let after = (pending_of("agent-hidden"), pending_of("agent-visible"));
        assert_eq!(
            before.0, after.0,
            "the unread agent's journal row must be untouched by the halted pass: \
             pending receipts stay pending, never rewritten as an empty read"
        );
        assert!(
            before.0.contains("\"bundles\":[\""),
            "the fixture must actually carry the agent's prior receipt set: {}",
            before.0
        );
        // The settled agent completed a fresh, proven pass: its journal row
        // advanced and holds no outstanding receipts, which is what the
        // halted pass must not fake for the unread one.
        let after_visible = serde_json::from_str::<Value>(&after.1).unwrap();
        assert_eq!(
            after_visible["required"].as_array().map(Vec::len),
            Some(0),
            "the settled agent proved again: {}",
            after.1
        );
        let halted = serde_json::from_str::<Value>(&after.0).unwrap();
        assert!(
            !halted["bundles"].as_array().unwrap().is_empty(),
            "the unread agent keeps its receipt set: {}",
            after.0
        );
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
