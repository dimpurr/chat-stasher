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
/// Smallest serial pacing the **declared path** (`[pull.letta]` under a
/// timer) accepts. An unattended pass has nobody watching it slow down, so a
/// declaration below one request per second would hammer exactly the
/// provider the pacing promise is meant to protect; the interactive command
/// keeps its wider band because a person is present and can stop it.
pub const MIN_PASS_PACE_MS: u64 = 1000;
/// Default whole-pass wall-clock budget, in seconds.
pub const DEFAULT_BUDGET_SECONDS: u64 = 600;
/// Largest accepted whole-pass budget: one hour (the interactive command).
pub const MAX_BUDGET_SECONDS: u64 = 3_600;
/// Largest whole-pass budget the **declared path** accepts: 45 minutes,
/// below the hourly period a timer runs on. A pass that may overrun its
/// own period contends on the state lock with the next pass, which is the
/// "exit 3 by construction" failure shape the cap refuses.
pub const PASS_MAX_BUDGET_SECONDS: u64 = 2_700;
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
    /// Serial request pacing in milliseconds (default ~1 request/second;
    /// the declared path accepts `MIN_PASS_PACE_MS..=MAX_PACE_MS`).
    pub pace_ms: u64,
    /// Whole-pass wall-clock budget in seconds
    /// (`1..=PASS_MAX_BUDGET_SECONDS` on the declared path).
    pub budget_seconds: u64,
    /// Rows requested per page (`MIN_PAGE_SIZE..=MAX_PAGE_SIZE`).
    pub page_size: u64,
    /// The `pull.letta.api_key` declaration as the config loader resolved
    /// it: a literal, or the secret a `file:` / `env-file:` / `keychain:`
    /// reference produced. `None` means nothing was declared and the pass
    /// falls back to `LETTA_API_KEY` in the process environment — the form
    /// a timer cannot see, which is exactly why declaring the key is
    /// better. Never logged.
    pub api_key: Option<String>,
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
    // The declared path runs unattended, so its band is deliberately tighter
    // than the interactive command's: pacing below one request per second
    // hammers the provider nobody is watching, and a budget that can reach
    // the timer's own period contends on the state lock with the next pass.
    let pace_ms = pull.pace_ms.unwrap_or(DEFAULT_PACE_MS);
    ensure!(
        (MIN_PASS_PACE_MS..=MAX_PACE_MS).contains(&pace_ms),
        "pull.letta.pace_ms = {pace_ms} is outside the accepted \
         {MIN_PASS_PACE_MS}–{MAX_PACE_MS} millisecond band an unattended pass obeys"
    );
    let budget_seconds = pull.budget_seconds.unwrap_or(DEFAULT_BUDGET_SECONDS);
    ensure!(
        (1..=PASS_MAX_BUDGET_SECONDS).contains(&budget_seconds),
        "pull.letta.budget_seconds = {budget_seconds} is outside the accepted \
         1–{PASS_MAX_BUDGET_SECONDS} second band an unattended pass obeys; a pass that could \
         reach its own period would contend with the next one"
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
    // The config loader resolved the credential reference before this, so the
    // secret is already in hand here — a timer-unreachable reference never
    // gets this far, it refused the config load with the key named.
    Ok(Some(PassPull {
        account,
        inbox: PathBuf::from(inbox),
        state,
        pace_ms,
        budget_seconds,
        page_size,
        api_key: pull
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    }))
}

/// Another pass of this machine holds the producer's state lock. This is not
/// a failure of the pull pipeline — with one declared destination per timer,
/// the passes share one producer state, so all but the pass that won the
/// lock see this every hour, and the winner reports the producer's own
/// result that round. The scheduled pass therefore records a **deferral**
/// (`run-state.json`'s `pull_letta_deferred`, status's producer line)
/// instead of pretending to have read; the standalone `pull letta` command
/// still refuses with the same sentence, because a manual invocation has
/// nothing to defer to.
#[derive(Debug)]
pub struct ProducerBusy;
impl std::fmt::Display for ProducerBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Letta producer state busy")
    }
}
impl std::error::Error for ProducerBusy {}

pub struct Api {
    client: reqwest::blocking::Client,
    base: String,
    key: Option<String>,
    deadline: Instant,
    pace: Duration,
    /// The whole-pass budget as declared, kept beside the derived deadline
    /// so the pass can measure its request allowance in pacing intervals
    /// without consulting the running clock.
    budget: Duration,
    next: Instant,
    response_bytes: usize,
    page_size: u64,
}
impl Api {
    /// The credential a caller already resolved (the `[pull.letta].api_key`
    /// declaration), or the `LETTA_API_KEY` environment variable when the
    /// caller has none. Both failures are the same one sentence, because
    /// both mean the pull cannot authenticate — the difference between "no
    /// declaration" and "a declaration that resolved to nothing" is not a
    /// fact the user can act on differently.
    pub fn environment(
        resolved: Option<String>,
        pace: Duration,
        budget: Duration,
        page_size: u64,
    ) -> Result<Self> {
        let key = match resolved {
            Some(key) if !key.trim().is_empty() => key,
            _ => std::env::var("LETTA_API_KEY")
                .map_err(|_| anyhow::anyhow!("Letta credential unavailable"))?,
        };
        ensure!(!key.trim().is_empty(), "Letta credential unavailable");
        Self::new("https://api.letta.com", Some(key), pace, budget, page_size)
    }
    /// The whole-pass deadline every phase of a run shares: requests wait
    /// against it, and the proof phase is bounded by it too, so no phase of
    /// a pass can silently run past the budget the pass was given.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// The whole-pass request budget, measured in pacing intervals. Both
    /// numbers are declared before the first request, so the answer is the
    /// same however slow the machine is while it runs — the derived quantity
    /// a load-slowed pass may still plan against, where the wall clock that
    /// remains is not one.
    fn request_quanta(&self) -> usize {
        (self.budget.as_millis() / std::cmp::max(1, self.pace.as_millis().max(1))) as usize
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
            budget,
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
#[derive(Default, Clone, Serialize, Deserialize)]
struct Snapshot {
    sessions: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    bundles: Vec<String>,
    #[serde(default)]
    required: BTreeSet<String>,
    /// Per-session content cursors of the last published round: session →
    /// bundle class → cursor. A cursor is a digest of the content the
    /// bundles of that class carry — the raw the `file` block hashes — and
    /// never of the bundle wrapper, whose `capturedAt` changes every pass,
    /// so an unchanged conversation produces the same cursor forever.
    /// Where every cursor of a session matches its published predecessor,
    /// the round republishes nothing.
    #[serde(default)]
    cursors: BTreeMap<String, BTreeMap<String, String>>,
    /// Where the re-verification sweep of this agent resumes: the first
    /// conversation the previous round did not re-read. A round re-reads as
    /// many conversations as its pace budget admits, starting here, so a
    /// caught-up account costs a bounded slice of message fetches per round
    /// instead of the whole account — and correctness never rests on a
    /// conversation's listing row saying "unchanged": whatever the row
    /// claims, the sweep re-reads the conversation and re-diffs the content
    /// cursors within a bounded number of rounds.
    #[serde(default)]
    verify: BTreeMap<String, String>,
    /// This session's contribution to the capture-order record, from the
    /// last walk that fetched it. A steady round assembles the record from
    /// these pieces, so a round that skipped every fetch republishes a
    /// byte-identical capture order instead of an emptied one.
    #[serde(default)]
    orders: BTreeMap<String, String>,
    /// Conversations of this agent whose first publication is still owed,
    /// plus the agent-level marker (`agent-default-<agent>`), which leaves
    /// the set only when the round that journals the default-session
    /// material — window, listing, metadata, capture order — commits: that
    /// is the state where the position empties, never the last
    /// conversation's publication. `None` therefore means the account is
    /// caught up and every round walks the inventory's slice against the
    /// cursors above; `Some(pending)` means the round starts where the
    /// previous one stopped (a conversation already present stays in the
    /// set until its bundles are both published and durably journaled),
    /// which is how a large account completes across several rounds instead
    /// of restarting from zero and staying unfinished forever — a
    /// marker-only position included, because the agent-level material is
    /// publications the account still owes.
    #[serde(default)]
    backlog: Option<BTreeSet<String>>,
}

/// Backfill progress, while the account is still catching up across rounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backfill {
    /// Conversations fully published and journaled so far.
    pub pulled: usize,
    /// Conversations the listing says exist. An estimate for progress
    /// display, never a claim that nothing new appeared while backfilling.
    pub estimated_total: usize,
}

#[derive(Default)]
pub struct Report {
    pub agents: usize,
    pub local_only: usize,
    pub bundles: usize,
    pub committed: usize,
    pub incomplete: usize,
    /// Some while the account still owes publications — conversations
    /// awaiting their first publication, or the agent-level material of a
    /// scope whose conversations are all published and whose window,
    /// listing and metadata are not. The pass did not finish reading, and
    /// the next round continues from the persisted position. None means
    /// the account is caught up: a round journaled everything it owed.
    pub backfill: Option<Backfill>,
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

/// Replay the inventory as a paced, resumable sweep — edit and disappearance
/// detection is a content comparison, never a metadata shortcut — but let
/// publication follow the content: an unchanged conversation republishes
/// nothing, and a conversation the round cannot reach stays in the backfill
/// position and is continued by the next round. A round re-reads as many
/// conversations as its pace budget admits (the sweep's slice), resuming
/// where the previous round stopped, so every conversation is content-
/// verified within a bounded number of rounds rather than on the word of
/// its listing row. Window exhaustion cannot establish absence for
/// agent-only messages.
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
        .map_err(|_| anyhow::Error::new(ProducerBusy))?;
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
    // The deadline every phase of this round shares: each request waits
    // against it inside [`Api`], the walk checks it before reaching for a
    // new conversation, and the proof phase is bounded by the same moment —
    // no phase of a round can silently outgrow the budget it was given,
    // which is what keeps a scheduled pass inside its period.
    let deadline = api.deadline();
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
    // Backfill bookkeeping: the conversation inventory, grouped by agent, is
    // both the walk order and the progress estimate the rounds that
    // complete a large account across several passes report.
    let mut per_agent_conversations: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for conversation in &conversations {
        let agent = safe(
            conversation["agent_id"]
                .as_str()
                .context("conversation agent missing")?,
        )?;
        per_agent_conversations
            .entry(agent.to_owned())
            .or_default()
            .insert(id(conversation)?.to_owned());
    }
    let total_conversations: usize = per_agent_conversations.values().map(BTreeSet::len).sum();
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
    // The sweep's per-round slice: how many conversations a caught-up round
    // re-reads before handing the rest to the next round. Derived from the
    // pass's **declared** request budget — the whole-pass budget in pacing
    // intervals ([`Api::request_quanta`]), never the clock remaining at the
    // moment the slice is computed — so a load-slowed round plans the same
    // slice a quiet one does instead of collapsing toward one conversation
    // per agent. The slice takes a quarter of the round's requests and
    // shares that quarter across the account's agents: a conversation costs
    // at least one paced request for its content page and one for the empty
    // terminator that ends paging, and each agent still owes its metadata
    // and window, so the slices add up inside the budget the round was
    // given rather than onto the wall clock the next agent trips over. A
    // deadline that strikes inside the slice still reports "did not finish
    // reading" and resumes from the persisted position.
    let verify_slice: usize =
        std::cmp::max(1, api.request_quanta() / 4 / std::cmp::max(1, known.len()));
    for agent in known.iter().cloned() {
        let scope = format!("{tenant}/{agent}");
        let mut pass_proven = false;
        let result = (|| -> Result<()> {
            let metadata = api.get(&format!("/v1/agents/{agent}"), &[])?;
            ensure!(id(&metadata)? == agent, "agent metadata identity mismatch");
            let row = db
                .query_row("SELECT pending FROM agents WHERE scope=?1", [&scope], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?;
            // `existed` distinguishes a scope with no journal row — which
            // owes every conversation it has — from a legacy row written
            // before backfill tracking, which is caught up by definition:
            // its journal carries a full inventory's receipts already.
            let (existed, mut previous) = match row {
                Some(serialized) => (true, serde_json::from_str::<Snapshot>(&serialized)?),
                None => (false, Snapshot::default()),
            };
            // A prior journal describes an independently completed source
            // pass. Settle it after push, before emitting fresh capture times;
            // otherwise a continuously changing source could never advance.
            if !previous.required.is_empty() {
                crate::test_identity_guard::refuse_fixture_write(&[machine], stage)?;
                fs::create_dir_all(stage)?;
                if prove_receipts(&previous.required, inbox, stage, machine, proof, deadline)? {
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
            // The round's working snapshot. The published-content cursors,
            // the listing-row cursors, the capture-order pieces and the
            // backfill position carry over from the settled journal; the
            // session memory and the ledgers are rebuilt by this round's
            // walk (a backfill round derives only what it walks, a
            // caught-up round walks the rows that moved), so a session that
            // left the source is observed as absent and a round that stops
            // part-way journals exactly what it reached. The backfill
            // position inside the snapshot is the live working set: every
            // publication removes its conversation from it before the
            // journal commits, so the persisted position is always the one
            // the next round continues from.
            let mut snapshot = Snapshot {
                cursors: previous.cursors.clone(),
                verify: previous.verify.clone(),
                orders: previous.orders.clone(),
                backlog: previous.backlog.clone(),
                ..Snapshot::default()
            };
            // The default session's id is also the backlog's agent-level
            // marker: the conversations are tracked per id, and the scope's
            // window, listing, metadata and capture order are tracked under
            // this one name.
            let default = format!("agent-default-{agent}");
            // The backfill position: a scope with no row owes every
            // conversation it has plus its agent-level material; a scope
            // mid-backfill continues the set it persisted; every other
            // scope is caught up.
            if !existed {
                let mut pending = per_agent_conversations
                    .get(&agent)
                    .cloned()
                    // reason: an agent with no row in the conversation
                    // listing has no conversations to owe — an empty
                    // conversation set is the position to continue from,
                    // not a fallback standing in for an unknown one.
                    .unwrap_or_default();
                // The agent-level marker: every conversation may publish
                // while the default-session material is still unwritten, so
                // the position never empties before the round that journals
                // that material. An emptied position read as completion was
                // the rev3 hole — a round could persist "no conversations
                // owed" and the next computation report the account caught
                // up while its window, listing and metadata were never
                // fetched.
                pending.insert(default.clone());
                snapshot.backlog = Some(pending);
            }
            let mut conversation_ids = BTreeSet::new();
            let mut overlap = BTreeSet::new();
            // The sweep's order: this agent's conversations in listing
            // order, rotated so the first one is where the previous round's
            // slice stopped. The resume point is an id, and a source that
            // drops or reorders it simply restarts the sweep at the
            // listing's start — a rotation may loop a second time, it can
            // never skip a conversation for good.
            let agent_rows: Vec<usize> = conversations
                .iter()
                .enumerate()
                .filter(|(_, row)| row["agent_id"] == agent)
                .map(|(index, _)| index)
                .collect();
            let start = previous
                .verify
                .get(&agent)
                .and_then(|at| {
                    agent_rows.iter().position(|&index| {
                        id(&conversations[index]).is_ok_and(|session| session == at)
                    })
                })
                // reason: a resume id the listing no longer names means the
                // source moved or dropped that conversation — starting the
                // sweep at the listing's head is the honest fallback, not a
                // fabricated position; the only cost is re-reading what an
                // earlier round already verified, never skipping one.
                .unwrap_or(0);
            let ordered: Vec<usize> = agent_rows[start..]
                .iter()
                .chain(agent_rows[..start].iter())
                .copied()
                .collect();
            // The slice: a caught-up round re-reads exactly `verify_slice`
            // conversations before handing the rest to the next round. Mid
            // backfill there is no slice — that round owes first
            // publication, and the deadline alone bounds how far it gets.
            // A position that holds only the agent-level marker owes no
            // conversation walks, so it gets the caught-up slice (never an
            // unbounded one): a scope that walked everything under
            // `usize::MAX` was the other half of the rev3 hole.
            let verify_budget = match &snapshot.backlog {
                Some(pending) if pending.iter().any(|session| *session != default) => usize::MAX,
                _ => verify_slice,
            };
            let mut reverified: usize = 0;
            let mut last_read: Option<String> = None;
            for &row_index in &ordered {
                let conversation = &conversations[row_index];
                let session = id(conversation)?;
                ensure!(
                    !session.starts_with("agent-default-"),
                    "reserved default session collision"
                );
                if !conversation_ids.insert(session.to_owned()) {
                    continue;
                }
                if let Some(pending) = &snapshot.backlog {
                    if !pending.contains(session) {
                        // Published by an earlier round of the backfill: the
                        // session memory and cursors describe it, the walk
                        // continues from the position it persisted, and the
                        // budget that conversation once cost belongs to the
                        // conversations still owed.
                        if let Some(old) = previous.sessions.get(session) {
                            snapshot.sessions.insert(session.to_owned(), old.clone());
                        }
                        continue;
                    }
                }
                if reverified >= verify_budget {
                    // The round's slice is spent: the conversation keeps
                    // the memory of its last re-verified state — carried,
                    // never claimed as current — and the sweep reaches it
                    // within a bounded number of rounds whatever its
                    // listing row says. This is the one place memory
                    // outlives a round, so the carry is bounded by the
                    // sweep, not by the source's word.
                    if let Some(old) = previous.sessions.get(session) {
                        snapshot.sessions.insert(session.to_owned(), old.clone());
                    }
                    continue;
                }
                // The budget gate before reaching for a new conversation. A
                // round that cannot reach a conversation leaves it pending —
                // never reported as read — and the next round continues from
                // here instead of restarting from zero.
                ensure!(Instant::now() < deadline, "Letta pass budget exhausted");
                let rows =
                    api.pages(&format!("/v1/conversations/{session}/messages"), &[], false)?;
                snapshot.orders.insert(
                    session.to_owned(),
                    observation_order(session, "conversation", &rows)?,
                );
                let (raw, variants) = messages(&rows)?;
                overlap.extend(variants.keys().cloned());
                snapshot.sessions.insert(session.into(), variants);
                let status = match conversation["archived"].as_bool() {
                    Some(true) => vec!["archived"],
                    Some(false) => vec!["active"],
                    None => vec![],
                };
                let complete_metadata: String = conversations
                    .iter()
                    .filter(|row| row["id"] == session)
                    .map(|row| crate::import::canonical_json(row) + "\n")
                    .collect();
                let recorded = snapshot.cursors.entry(session.to_owned()).or_default();
                recorded.insert("messages".to_owned(), hash(raw.as_bytes()));
                recorded.insert(
                    "conversation-metadata".to_owned(),
                    hash(complete_metadata.as_bytes()),
                );
                reverified += 1;
                last_read = Some(session.to_owned());
                // Two content cursors for the two conversation classes: the
                // raw the `file` block hashes and the metadata rows —
                // everything except the bundle wrapper, whose `capturedAt`
                // moves every pass. Where both match the published
                // predecessors and the prior round settled its receipts, the
                // conversation publishes nothing this round; one that moved
                // republishes exactly its own pair.
                let unchanged = previous.cursors.get(session).is_some_and(|prior| {
                    prior.get("messages").is_some_and(|value| {
                        value == &hash(raw.as_bytes())
                            && prior.get("conversation-metadata").is_some_and(|metadata| {
                                metadata == &hash(complete_metadata.as_bytes())
                            })
                    })
                });
                if !unchanged {
                    publish(
                        inbox,
                        &mut report,
                        &mut snapshot,
                        &agent,
                        &tenant,
                        session,
                        "messages",
                        &raw,
                        status.clone(),
                        false,
                    )?;
                    publish(
                        inbox,
                        &mut report,
                        &mut snapshot,
                        &agent,
                        &tenant,
                        session,
                        "conversation-metadata",
                        &complete_metadata,
                        status,
                        false,
                    )?;
                    if let Some(pending) = &mut snapshot.backlog {
                        pending.remove(session);
                    }
                }
                // Durable before the walk reaches for the next conversation,
                // published or not — a re-verified conversation that stayed
                // unchanged is the sweep's own progress, and the next round
                // must resume from it rather than re-reading what this one
                // already proved.
                db.execute(
                    "INSERT INTO agents VALUES (?1,NULL,?2) ON CONFLICT(scope) DO UPDATE SET pending=excluded.pending",
                    rusqlite::params![scope, serde_json::to_string(&snapshot)?],
                )?;
                db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            }
            // The sweep position the next round resumes from: the first
            // listed conversation after the last one this round read, or the
            // listing's start when the read reached the end — the rotation
            // cycles rather than completing, so every conversation is
            // re-read within a bounded number of rounds however large the
            // account grows.
            if let Some(last) = last_read {
                let read_at = agent_rows.iter().position(|&index| {
                    id(&conversations[index]).is_ok_and(|session| session == last)
                });
                if let Some(at) = read_at {
                    let next = agent_rows[(at + 1) % agent_rows.len()];
                    if let Ok(resume) = id(&conversations[next]).map(str::to_owned) {
                        snapshot.verify.insert(agent.clone(), resume);
                    }
                }
            }
            // A conversation that left the source during a backfill can no
            // longer be first-published; the listing this round decides what
            // is still there, so the position drops what is gone instead of
            // owing it forever. Its bytes were never archived, so the
            // observation surface stays honest: there is nothing recorded to
            // compare against, and none is invented. The agent-level marker
            // is not a conversation and never leaves with the inventory — it
            // leaves when the round that journals the default-session
            // material sets the position caught up.
            if let Some(pending) = &mut snapshot.backlog {
                let inventory = per_agent_conversations.get(&agent);
                pending
                    .retain(|id| *id == default || inventory.is_some_and(|set| set.contains(id)));
            }
            if snapshot
                .backlog
                .as_ref()
                .is_some_and(|pending| pending.iter().any(|session| *session != default))
            {
                // The budget stopped this agent mid-backfill: its remaining
                // conversations stay pending, its agent-level material is
                // unwritten, and the round that finishes the walk publishes
                // the rest.
                db.execute(
                    "INSERT INTO agents VALUES (?1,NULL,?2) ON CONFLICT(scope) DO UPDATE SET pending=excluded.pending",
                    rusqlite::params![scope, serde_json::to_string(&snapshot)?],
                )?;
                db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
                report.incomplete += 1;
                return Ok(());
            }
            // The conversations' part of the position has cleared; the
            // marker leaves with the journal that publishes the
            // default-session material below — only that journal makes
            // "caught up" true, and it is the snapshot (not a local
            // variable) that persists.
            snapshot.backlog = None;
            let rows = api.pages(&format!("/v1/agents/{agent}/messages"), &[], true)?;
            snapshot.orders.insert(
                default.clone(),
                observation_order(&default, "agent-window", &rows)?,
            );
            // Same-id different renderings on the agent route belong to their
            // observed conversation too; preserve them instead of discarding.
            let extras: Vec<Value> = rows
                .iter()
                .filter(|r| id(r).is_ok_and(|identified| !overlap.contains(identified)))
                .cloned()
                .collect();
            let (default_raw, variants) = messages(&extras)?;
            snapshot.sessions.insert(default.clone(), variants);
            let listing: String = agents
                .iter()
                .filter(|row| row["id"] == agent)
                .map(|row| crate::import::canonical_json(row) + "\n")
                .collect();
            let metadata_raw = crate::import::canonical_json(&metadata) + "\n";
            // Retain the complete window, including overlapping renderings.
            let (window_raw, _) = messages(&rows)?;
            // The capture order assembles from the per-session pieces the
            // walks recorded — conversations in listing order, then the
            // agent window. The pieces carry across rounds in the journal,
            // so a round that skipped every fetch (nothing moved at the
            // source) re-emits a byte-identical record whose cursor
            // matches, and a round that clears a multi-round backfill
            // re-emits the complete record from the pieces the earlier
            // rounds left beside their publications.
            let mut capture_order = String::new();
            for conversation in conversations.iter().filter(|c| c["agent_id"] == agent) {
                let session = id(conversation)?;
                if !conversation_ids.contains(session) {
                    continue;
                }
                if let Some(order) = snapshot.orders.get(session) {
                    capture_order.push_str(order);
                }
            }
            if let Some(order) = snapshot.orders.get(&default) {
                capture_order.push_str(order);
            }
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
            // Agent-level classes, each under its own content cursor: the
            // default-session extras, the listing and metadata rows, the
            // complete window, and the capture order. A steady round where
            // nothing moved republishes none of them.
            let agent_level: [(&str, &str, String); 5] = [
                (
                    "messages",
                    default_raw.as_str(),
                    hash(default_raw.as_bytes()),
                ),
                ("agent-listing", listing.as_str(), hash(listing.as_bytes())),
                (
                    "agent-metadata",
                    metadata_raw.as_str(),
                    hash(metadata_raw.as_bytes()),
                ),
                (
                    "agent-window",
                    window_raw.as_str(),
                    hash(window_raw.as_bytes()),
                ),
                (
                    "capture-order",
                    capture_order.as_str(),
                    hash(capture_order.as_bytes()),
                ),
            ];
            // A round that clears a multi-round backfill assembles its
            // agent-level classes from live window rows plus the carried
            // pieces, so what it publishes is complete for the exact same
            // reason a full round's is.
            for (class, raw, cursor) in agent_level {
                let unchanged = previous.required.is_empty()
                    && previous
                        .cursors
                        .get(&default)
                        .and_then(|prior| prior.get(class))
                        == Some(&cursor);
                snapshot
                    .cursors
                    .entry(default.clone())
                    .or_default()
                    .insert(class.to_owned(), cursor);
                if !unchanged {
                    publish(
                        inbox,
                        &mut report,
                        &mut snapshot,
                        &agent,
                        &tenant,
                        &default,
                        class,
                        raw,
                        vec![],
                        true,
                    )?;
                }
            }
            if !observations.is_empty() {
                let cursor = observation_cursor(&observations);
                let unchanged = previous.required.is_empty()
                    && previous
                        .cursors
                        .get(&default)
                        .and_then(|prior| prior.get("observations"))
                        == Some(&cursor);
                snapshot
                    .cursors
                    .entry(default.clone())
                    .or_default()
                    .insert("observations".to_owned(), cursor);
                if !unchanged {
                    publish(
                        inbox,
                        &mut report,
                        &mut snapshot,
                        &agent,
                        &tenant,
                        &default,
                        "observations",
                        &observations,
                        vec![],
                        true,
                    )?;
                }
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
            db.execute("INSERT INTO agents VALUES (?1,NULL,?2) ON CONFLICT(scope) DO UPDATE SET pending=excluded.pending", rusqlite::params![scope, serde_json::to_string(&snapshot)?])?;
            db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            let proven =
                prove_receipts(&snapshot.required, inbox, stage, machine, proof, deadline)?;
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
    // Backfill progress: what the listing says exists minus what the scopes'
    // journals still owe. A scope this round never reached has no row, so
    // its conversations count as owed — `status` words the difference as
    // pulled-so-far of an estimated total, never a completion claim. A
    // scope whose journal still holds a position at all — conversations
    // pending, or only the agent-level marker, which is the state a round
    // that published every conversation but could not reach its window
    // leaves — has not completed, whatever its pending set counts: "caught
    // up" is what a round that journals everything may claim, not what an
    // emptied conversation position implies.
    let mut remaining = 0usize;
    let mut owes_agent_phase = false;
    for agent_id in &known {
        let scope = format!("{tenant}/{agent_id}");
        let row = db
            .query_row("SELECT pending FROM agents WHERE scope=?1", [&scope], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        match row {
            None => {
                remaining += per_agent_conversations
                    .get(agent_id)
                    .map_or(0, |set| set.len());
            }
            Some(serialized) => {
                let snapshot = serde_json::from_str::<Snapshot>(&serialized)?;
                if let Some(pending) = snapshot.backlog {
                    // Conversations are the counted unit; the marker is the
                    // agent-level material's own line in the position, so a
                    // marker-only journal keeps `pulled` at the conversation
                    // truth instead of inventing an owed conversation.
                    remaining += pending
                        .iter()
                        .filter(|session| !session.starts_with("agent-default-"))
                        .count();
                    owes_agent_phase = true;
                }
            }
        }
    }
    if remaining > 0 || owes_agent_phase {
        report.backfill = Some(Backfill {
            pulled: total_conversations.saturating_sub(remaining),
            estimated_total: total_conversations,
        });
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
    deadline: Instant,
) -> Result<bool> {
    if required.is_empty() {
        return Ok(true);
    }
    // The proof phase is bounded by the same deadline the requests obey: a
    // round whose budget the walk left alone can still prove what it
    // sealed, but no phase of a round can borrow unbounded time from the
    // next pass — a scheduled pass that overruns its period contends on
    // the state lock with its own successor, which is the frozen-forever
    // failure shape the deadline exists to refuse.
    ensure!(Instant::now() < deadline, "Letta proof budget exhausted");
    let mut outcomes = Vec::with_capacity(required.len());
    for digest in required {
        let name = format!("letta-{digest}.json");
        let bytes = fs::read(inbox.join(&name))
            .or_else(|_| fs::read(inbox.join("consumed").join(&name)))
            .context("pending bundle unavailable")?;
        ensure!(hash(&bytes) == *digest, "pending bundle digest mismatch");
        let outcome = crate::inbox::seal_payload(&name, &bytes, stage, machine, 1000, None, None)
            .map_err(|_| anyhow::anyhow!("Letta seal unavailable"))?;
        outcomes.push(outcome);
    }
    // One proof query for the whole set: each destination is asked about
    // every receipt in one batch, so a repository is opened once per
    // destination per round, never once per bundle.
    ensure!(Instant::now() < deadline, "Letta proof budget exhausted");
    Ok(proof
        .holds_many(machine, &outcomes)?
        .iter()
        .all(|held| *held))
}

/// Publish one bundle: an inbox@3 file named by its digest, plus the
/// journal entry (`required`, `bundles`) this round's archive proof settles
/// against. `capturedAt` lives in the bundle wrapper and never enters a
/// cursor, so two publications of identical content are identical only
/// in what the cursors compare — which is the point of the cursors.
#[allow(clippy::too_many_arguments)]
fn publish(
    inbox: &Path,
    report: &mut Report,
    snapshot: &mut Snapshot,
    agent: &str,
    tenant: &str,
    session: &str,
    class: &str,
    raw: &str,
    status: Vec<&str>,
    window: bool,
) -> Result<()> {
    let bytes = bundle(tenant, agent, session, class, raw, status, window)?;
    let digest = hash(&bytes);
    atomic(&inbox.join(format!("letta-{digest}.json")), &bytes)?;
    snapshot.required.insert(digest.clone());
    if !snapshot.bundles.contains(&digest) {
        snapshot.bundles.push(digest);
    }
    report.bundles += 1;
    Ok(())
}

/// The observations cursor: the content of the observation lines with the
/// moment-of-observation field removed, so a repeated observation — a
/// conversation still absent at source — is not republished every round
/// only because its timestamp moved. Nothing else in an observation line
/// is time-dependent.
fn observation_cursor(observations: &str) -> String {
    let mut canonical = String::new();
    for line in observations.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                if let Some(map) = value.as_object_mut() {
                    map.remove("observed_at");
                }
                canonical.push_str(&value.to_string());
                canonical.push('\n');
            }
            Err(_) => continue,
        }
    }
    hash(canonical.as_bytes())
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
        // Three proven rounds of an unchanged source: the first publishes
        // the agent's bundles, the later two publish nothing — a round the
        // cursors match is a no-op, which is the property that keeps a
        // caught-up account's steady state at zero re-sends.
        let first = pull(true);
        assert_eq!(first.committed, 1);
        assert!(first.bundles > 0, "the first round publishes the agent");
        let snapshot = pending();
        assert_eq!(snapshot["bundles"].as_array().unwrap().len(), first.bundles);
        for _ in 0..2 {
            let report = pull(true);
            assert_eq!(report.committed, 1);
            assert_eq!(
                report.bundles, 0,
                "an unchanged proven source republishes nothing"
            );
        }
        // A source that changed under an unprovable proof leaves fresh
        // receipts pending — the one state that must record incomplete.
        server.mode.store(1, Ordering::SeqCst);
        assert_eq!(pull(false).incomplete, 1);
        server.mode.store(0, Ordering::SeqCst);
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
        // The settling round proves the pending receipts and then proves
        // the source is unchanged by its cursors: complete, with the
        // journal exactly as small as the changed content.
        assert_eq!(pull(true).committed, 1);
        assert_eq!(pull(true).incomplete, 0);
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
                    // A client that aborts mid-exchange is one connection
                    // to drop, not a panic: a pass whose budget runs out
                    // abandons exactly one in-flight request, which is
                    // the behavior these tests exercise on purpose. The
                    // abort shows up here as EOF, a reset, or a read
                    // timeout — never as a well-formed request.
                    let count = match stream.read(&mut input) {
                        Ok(count) if count > 0 => count,
                        Ok(_) | Err(_) => continue,
                    };
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
                    // The conversation listing row changes with the modes
                    // that change the conversation — that is how a
                    // provider's row *usually* says "this conversation
                    // moved", and mode 14 exists because that word is not
                    // proof: it serves edited content under a row that
                    // stays byte-identical, the case a walk must verify by
                    // re-reading, not trust.
                    let row_updated_at = if mode == 1 {
                        "2026-02-02T00:00:00Z"
                    } else if mode == 2 {
                        "2026-03-03T00:00:00Z"
                    } else {
                        "2026-01-01T00:00:00Z"
                    };
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
                    } else if (mode == 12
                        && path.starts_with("/v1/agents/?")
                        && g.swap(1, Ordering::SeqCst) == 0)
                        || (path.starts_with("/v1/agents/agent-visible/messages")
                            && r.fetch_add(1, Ordering::SeqCst) == 0)
                    {
                        status = "429 Too Many Requests";
                        headers = "Retry-After: 1\r\n";
                        json!({})
                    } else if path.starts_with("/v1/agents/?") {
                        if after {
                            json!([])
                        } else if mode == 13 {
                            json!([{"id":"agent-big"}])
                        } else {
                            json!([{"id":"agent-visible"}])
                        }
                    } else if path.starts_with("/v1/conversations/?") {
                        assert!(path.contains("archive_status=all"));
                        if mode == 13 {
                            // The oversized-account fixture: one agent, many
                            // conversations, all changing nothing between
                            // rounds, so the backfill spans rounds on budget
                            // and the steady rounds re-verify a bounded
                            // slice each.
                            if after {
                                json!([])
                            } else {
                                let conversations: Vec<Value> = (1..=90)
                                    .map(|n| {
                                        json!({"id":format!("conv-{n:02}"),
                                               "agent_id":"agent-big",
                                               "archived":false,
                                               "updated_at":"2026-01-01T00:00:00Z"})
                                    })
                                    .collect();
                                json!(conversations)
                            }
                        } else if after {
                            json!([])
                        } else {
                            json!([{"id":"conv-one","agent_id":"agent-visible","archived":true,"is_subagent":true,"unknown":null,"updated_at":row_updated_at}])
                        }
                    } else if mode == 13 && path.starts_with("/v1/conversations/conv-") {
                        // One message page plus its terminator per
                        // conversation: two paced requests each (plus the
                        // durable publication commits), the cost the backfill
                        // rounds are budgeted against.
                        if after {
                            json!([])
                        } else {
                            json!([{"id":"message-a","seq_id":1,"content":"synthetic"}])
                        }
                    } else if path.starts_with("/v1/conversations/conv-one/messages") {
                        if (after && path.contains("message-b")) || mode == 2 {
                            json!([])
                        } else if after {
                            json!([{"id":"message-a","seq_id":1,"content":"synthetic","render":true},{"id":"message-b","seq_id":2,"content":"synthetic"}])
                        } else {
                            json!([{"id":"message-a","seq_id":1,"content":if mode==1 || mode==14 {"synthetic edit"} else {"synthetic"},"unknown":{"null":null}}])
                        }
                    } else if mode == 15 && limit.is_some() && path.contains("/messages?") {
                        // A source that can serve every conversation but
                        // not the agent-level window: the regime where a
                        // round can finish all first publications and still
                        // owe its default-session material — the state that
                        // must not report itself as caught up.
                        status = "500 Internal Server Error";
                        json!({})
                    } else if limit.is_some() && path.contains("/messages?") {
                        if after {
                            json!([])
                        } else {
                            json!([{"id":"message-window","seq_id":3,"tool_returns":[{"unknown":null}]}])
                        }
                    } else if path.starts_with("/v1/agents/agent-") {
                        json!({"id":if mode == 13 {"agent-big"} else if path.contains("agent-hidden") {"agent-hidden"} else {"agent-visible"},"hidden":path.contains("agent-hidden"),"unknown":null})
                    } else {
                        panic!("unexpected mock route");
                    };
                    let body = serde_json::to_vec(&body).unwrap();
                    // Same tolerance for the reply: the peer may have
                    // aborted while the response was being written. A
                    // write that reached a live client is the client's
                    // failure domain — its own assertions and retries
                    // report it — so this thread only ends the connection.
                    let head = format!(
                        "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream
                        .write_all(head.as_bytes())
                        .and_then(|()| stream.write_all(&body))
                        .is_err()
                    {
                        continue;
                    }
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
        // A settle round whose proof holds exactly the prior receipts. The
        // property this pins is the ordering: those receipts are proven
        // *before* the round reaches for fresh captures, which succeed and
        // land as this round's own pending rows. An unchanged source would
        // re-derive the same cursors and publish nothing (the no-op a
        // caught-up account's steady state is), so the round is given a
        // source change to make the fresh captures real.
        server.mode.store(1, Ordering::SeqCst);
        let fresh = pull(&held);
        server.mode.store(0, Ordering::SeqCst);
        assert_eq!(fresh.committed, 2);
        // Only the agent whose content moved leaves fresh pending receipts;
        // the unchanged agent settles and then no-ops (nothing republished,
        // nothing to prove).
        assert_eq!(fresh.incomplete, 1);
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agents WHERE cursor IS NULL AND json_array_length(json_extract(pending, '$.required')) > 0", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            1
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

    /// The decision's steady-state promise: two rounds against an unchanged
    /// source, and the second publishes nothing — zero new inbox bytes, zero
    /// re-sends — while the round still completes (exit-shape incomplete 0),
    /// because the sweep re-verified the conversations it owed, found the
    /// content cursors matching and the capture order reassembled to
    /// identical bytes from its pieces.
    #[test]
    fn second_unchanged_round_republishes_nothing() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let server = Server::start();
        let pull = || {
            run(
                &mut server.api(),
                "synthetic-account",
                &local,
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
            .unwrap()
        };
        let first = pull();
        assert!(first.bundles > 0, "the first round must publish");
        assert_eq!(first.incomplete, 0);
        let inbox_files = || fs::read_dir(root.join("inbox")).unwrap().count();
        let requests_before = server.requests.load(Ordering::SeqCst);
        let before = inbox_files();
        let second = pull();
        assert_eq!(
            second.bundles, 0,
            "an unchanged source republishes nothing: no bytes, no re-sends"
        );
        assert_eq!(second.incomplete, 0, "the no-op round still completes");
        assert_eq!(second.committed, 2);
        assert_eq!(
            inbox_files(),
            before,
            "the unchanged round writes no publication bytes"
        );
        let steady_requests = server.requests.load(Ordering::SeqCst) - requests_before;
        assert!(
            steady_requests <= 16,
            "the steady round re-verifies the conversations its slice owes — \
             listings, windows and one re-read per conversation — not a \
             growing re-walk: {steady_requests} requests"
        );
        // And it stays a no-op on the third round too — the second round
        // re-derived nothing that could grow.
        assert_eq!(pull().bundles, 0);
        assert_eq!(inbox_files(), before);
    }

    /// The regression a metadata shortcut once hid: a provider that serves
    /// edited content while leaving the conversation's listing row
    /// byte-identical must not be archived as "unchanged". Correctness rests
    /// on comparing the fetched content, never on the listing row saying
    /// "nothing moved" — the row staying still is the exact case the walk
    /// must not trust, because nothing measures that the provider keeps the
    /// row in step with the content.
    #[test]
    fn content_changed_without_row_movement_is_still_captured() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        let local = root.join("agents");
        fs::create_dir_all(local.join("agent-hidden")).unwrap();
        let pull = || {
            run(
                &mut server.api(),
                "synthetic-account",
                &local,
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
            .unwrap()
        };
        let first = pull();
        assert!(first.bundles > 0, "the first round must publish");
        let steady = pull();
        assert_eq!(
            steady.bundles, 0,
            "the untouched source republishes nothing — the trap is armed"
        );
        // The trap: mode 14 serves edited content under a listing row that
        // stays byte-identical to the settled round's. Skipping the fetch on
        // the row alone would record the old content as "verified" — the
        // silent loss this test forbids.
        server.mode.store(14, Ordering::SeqCst);
        let edited = pull();
        assert!(
            edited.bundles > 0,
            "a conversation whose content changed underneath an unchanged \
             listing row must be re-read and republished"
        );
        let exports = captured(&root.join("inbox"));
        assert!(
            exports.iter().any(|v| v["raw"]["data"]
                .as_str()
                .unwrap()
                .contains("synthetic edit")),
            "the edited content is in the archive"
        );
        assert!(
            exports
                .iter()
                .any(|v| v["raw"]["data"].as_str().unwrap().contains("new version")),
            "the second variant is observed, not silently dropped"
        );
        server.mode.store(0, Ordering::SeqCst);
        // The revert is itself a content change the sweep must catch —
        // one round of re-publication — and the round after that settles:
        // detection is symmetric, and the sweep converges instead of
        // churning.
        assert!(pull().bundles > 0, "the reverted content is captured too");
        assert_eq!(pull().bundles, 0, "and the settled source is a no-op again");
    }

    /// A round that publishes every conversation but cannot reach the
    /// agent-level material (window, listing, metadata, capture order — mode
    /// 15 serves exactly the conversations and refuses the window) has
    /// **not** finished the account. "Caught up" is what the round that
    /// journals the agent-level material may claim, never what an emptied
    /// conversation position implies: the rev3 hole read the empty pending
    /// set as completion while five publications were still owed.
    #[test]
    fn caught_up_stays_false_until_the_agent_level_material_publishes() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        server.mode.store(15, Ordering::SeqCst);
        let pull = || {
            run(
                &mut server.api(),
                "synthetic-account",
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
            .unwrap()
        };
        // Phase one: every conversation publishes, the window route refuses,
        // and the round honestly says "did not finish" — with the account's
        // progress still reported, because the agent-level material is owed.
        let partial = pull();
        assert!(partial.bundles >= 2, "the conversation classes publish");
        assert_eq!(partial.incomplete, 1, "the round did not finish reading");
        assert!(
            partial.backfill.is_some(),
            "a round whose agent-level material is unwritten must not report \
             the account caught up: {:?}",
            partial.backfill
        );
        let conversations_published = fs::read_dir(root.join("inbox")).unwrap().count();
        // The durable position names what is owed: no conversations, and the
        // agent-level marker — not an emptied set that reads as completion.
        let db = rusqlite::Connection::open(root.join("state/letta.sqlite3")).unwrap();
        let serialized: String = db
            .query_row("SELECT pending FROM agents", [], |r| r.get(0))
            .unwrap();
        let snapshot = serde_json::from_str::<Value>(&serialized).unwrap();
        assert_eq!(
            snapshot["backlog"],
            json!(["agent-default-agent-visible"]),
            "the persisted position holds the agent-level marker, not an \
             emptied set: {serialized}"
        );
        // Phase two: the source serves the window again. The next round
        // publishes exactly the agent-level classes it still owes, publishes
        // no conversation bytes, and only now may claim caught up.
        server.mode.store(0, Ordering::SeqCst);
        let finishing = pull();
        assert_eq!(
            finishing.bundles, 5,
            "the finishing round publishes exactly the five agent-level \
             classes and no conversation bytes"
        );
        assert_eq!(finishing.incomplete, 0);
        assert_eq!(
            finishing.backfill, None,
            "caught up is the finishing round's claim, backed by the \
             journal it just wrote"
        );
        assert_eq!(
            fs::read_dir(root.join("inbox")).unwrap().count(),
            conversations_published + 5,
            "every publication happened exactly once: conversations in the \
             first round, agent-level material in the second"
        );
    }

    /// The decision's large-account promise: an account too big for one
    /// round's budget completes across several rounds, each continuing from
    /// the persisted position, reporting progress (`pulled` of the estimated
    /// total) while still honestly "did not finish reading" — and once
    /// caught up, each steady round re-verifies one bounded slice of the
    /// sweep at no-op cost.
    #[test]
    fn oversized_account_backfills_across_rounds_with_progress() {
        let sandbox = crate::test_support::Sandbox::new();
        let root = sandbox.root();
        let server = Server::start();
        server.mode.store(13, Ordering::SeqCst);
        // The fixture: one agent, ninety conversations, two paced requests
        // each plus the durable per-conversation commits — a whole first
        // pull costs about three of one round's 3200 ms budget at this
        // 50 ms pace, so the account provably spans several rounds. The
        // budget leaves the steady rounds half of their request budget
        // for the slice (a quarter of 64 request quanta = 16 conversations)
        // and the other half for jitter and the fixed route fetches, so a
        // steady round completes under ordinary load instead of tripping
        // its own deadline by construction.
        let pull = || {
            run(
                &mut Api::new(
                    &server.base,
                    None,
                    Duration::from_millis(50),
                    Duration::from_millis(3200),
                    DEFAULT_PAGE_SIZE,
                )
                .unwrap(),
                "synthetic-account",
                &root.join("agents"),
                &root.join("inbox"),
                &root.join("state"),
                &root.join("stage"),
                "synthetic-machine",
                &Proof(true),
            )
            .unwrap()
        };
        let mut rounds = 0usize;
        let mut pulled_so_far = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds <= 12,
                "the backfill must complete within a few rounds, still running after {rounds}"
            );
            let report = pull();
            match &report.backfill {
                Some(backfill) => {
                    // Monotone, not strictly increasing per round: a round
                    // that spends its whole budget part-way through one
                    // conversation is a legal zero-progress round under
                    // timing jitter; what the position guarantees is that
                    // it never goes backwards and that the total walk is
                    // bounded by the budget, not restarted.
                    assert!(
                        backfill.pulled >= pulled_so_far,
                        "a backfill round must never lose progress: \
                         {pulled_so_far} -> {}",
                        backfill.pulled
                    );
                    assert_eq!(backfill.estimated_total, 90);
                    assert_eq!(
                        report.incomplete, 1,
                        "a mid-backfill round did not finish reading"
                    );
                    pulled_so_far = backfill.pulled;
                }
                None => break,
            }
        }
        assert!(
            rounds >= 3,
            "the fixture is budgeted to span rounds, completed in {rounds}"
        );
        // Every conversation archived exactly once: two publication bundles
        // each, plus the agent-level set published by the clearing round.
        let published = fs::read_dir(root.join("inbox")).unwrap().count();
        assert_eq!(
            published,
            90 * 2 + 5,
            "each conversation archived exactly once"
        );
        // The steady rounds are no-ops that re-verify one bounded slice each:
        // the sweep rotates through the account a conversation at a time,
        // resuming where the previous round stopped, so repeated rounds
        // stay flat-cost instead of restarting the walk. Both slice sizes
        // are derived from the declared budget and pacing — never from the
        // clock that remains mid-round — so the two counts are equal
        // exactly, not within a load-sensitive tolerance.
        let requests_before = server.requests.load(Ordering::SeqCst);
        let steady = pull();
        assert_eq!(
            steady.bundles, 0,
            "the caught-up account republishes nothing"
        );
        assert_eq!(steady.incomplete, 0);
        assert_eq!(steady.backfill, None);
        assert_eq!(
            fs::read_dir(root.join("inbox")).unwrap().count(),
            published,
            "the steady round republishes nothing"
        );
        let steady_requests = server.requests.load(Ordering::SeqCst) - requests_before;
        let requests_after_first = server.requests.load(Ordering::SeqCst);
        let second_steady = pull();
        assert_eq!(second_steady.bundles, 0);
        assert_eq!(
            second_steady.incomplete, 0,
            "the next slice round completes too — the sweep resumes, it does not restart"
        );
        let second_steady_requests = server.requests.load(Ordering::SeqCst) - requests_after_first;
        assert!(
            steady_requests <= 40,
            "the steady round re-verifies one bounded slice of the account, \
             not a growing re-walk: {steady_requests} requests"
        );
        assert_eq!(
            second_steady_requests, steady_requests,
            "consecutive steady rounds cost the same slice — its size comes \
             from the declared budget and pacing, not from the clock, so the \
             counts are equal exactly, not within a tolerance: \
             {steady_requests} vs {second_steady_requests} requests"
        );
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
                        api_key: None,
                        pace_ms: None,
                        budget_seconds: None,
                        page_size: page,
                    }),
                }),
                ..Default::default()
            };
        // Absent section and empty `[pull]` table are both "not declared".
        assert!(pass_pull(&crate::config::Config::default())
            .unwrap()
            .is_none());
        assert!(pass_pull(&crate::config::Config {
            pull: Some(crate::config::PullSectionConfig::default()),
            ..Default::default()
        })
        .unwrap()
        .is_none());
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
        // The declared path runs unattended on a timer, so its band is
        // narrower than the interactive command's: pacing below one request
        // per second would hammer exactly the provider nobody is watching,
        // and a budget that can reach the timer's own period would contend
        // on the state lock with the next pass.
        for underpaced in [0u64, MIN_PASS_PACE_MS - 1] {
            let mut floored = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
            floored
                .pull
                .as_mut()
                .unwrap()
                .letta
                .as_mut()
                .unwrap()
                .pace_ms = Some(underpaced);
            assert!(
                error(floored).contains("pull.letta.pace_ms"),
                "an unattended pass may not pace below {MIN_PASS_PACE_MS} ms"
            );
        }
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
        for periodic in [PASS_MAX_BUDGET_SECONDS + 1, MAX_BUDGET_SECONDS] {
            let mut overlong = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
            overlong
                .pull
                .as_mut()
                .unwrap()
                .letta
                .as_mut()
                .unwrap()
                .budget_seconds = Some(periodic);
            assert!(
                error(overlong).contains("pull.letta.budget_seconds"),
                "an unattended pass may not budget up to or past the hourly period"
            );
        }
        // The floor and the cap are the declared path's own: the resolvers
        // accept them.
        let mut lawful = declared(Some("synthetic-account"), Some("/inbox"), Some(100));
        lawful
            .pull
            .as_mut()
            .unwrap()
            .letta
            .as_mut()
            .unwrap()
            .pace_ms = Some(MIN_PASS_PACE_MS);
        lawful
            .pull
            .as_mut()
            .unwrap()
            .letta
            .as_mut()
            .unwrap()
            .budget_seconds = Some(PASS_MAX_BUDGET_SECONDS);
        assert!(pass_pull(&lawful).unwrap().is_some());
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

    /// The pass's credential comes from the config's resolved
    /// `pull.letta.api_key` declaration when there is one, and from the
    /// environment only when there is not — the forms a timer can reach and
    /// the form it cannot, both reaching the same one pipeline.
    #[test]
    fn pass_pull_carries_the_resolved_credential_declaration() {
        let declared = |api_key: Option<&str>| crate::config::Config {
            pull: Some(crate::config::PullSectionConfig {
                letta: Some(crate::config::LettaPullConfig {
                    account_id: Some("synthetic-account".to_string()),
                    inbox: Some("/inbox".to_string()),
                    state: None,
                    api_key: api_key.map(str::to_owned),
                    pace_ms: None,
                    budget_seconds: None,
                    page_size: Some(100),
                }),
            }),
            ..Default::default()
        };
        assert_eq!(
            pass_pull(&declared(None)).unwrap().unwrap().api_key,
            None,
            "no declaration means the environment fallback, carried as None"
        );
        assert_eq!(
            pass_pull(&declared(Some("  "))).unwrap().unwrap().api_key,
            None,
            "a blank declaration resolves to nothing, never to a blank credential"
        );
        // The config loader resolves references before the pass sees them,
        // so what arrives here is already the secret — never logged, only
        // passed to the API client.
        assert_eq!(
            pass_pull(&declared(Some("synthetic-resolved-secret")))
                .unwrap()
                .unwrap()
                .api_key
                .as_deref(),
            Some("synthetic-resolved-secret")
        );
        // And the client accepts a resolved credential without touching the
        // environment at all.
        assert!(
            Api::environment(
                Some("synthetic-resolved-secret".to_owned()),
                Duration::from_secs(1),
                Duration::from_secs(10),
                DEFAULT_PAGE_SIZE,
            )
            .is_ok(),
            "a resolved declaration is a usable credential without the environment"
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
