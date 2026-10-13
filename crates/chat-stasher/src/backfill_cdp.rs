//! The production [`AppDriver`] for `backfill grok-bot`: the desktop app,
//! relaunched over a loopback Chromium debugging port, driven by ordinary
//! UI clicks and read through view state — the transport the 2026-10-12
//! probe proved end to end, as a compile-checked implementation.
//!
//! Everything about the mechanics lives here so the run above it stays
//! pure orchestration:
//!
//! * the app is opened with `--remote-debugging-port=<port>` (the Node
//!   inspector flags are fuse-disabled in the packaged app; the Chromium
//!   debugging port is not — measured, attempts 2 and 3 of the probe);
//! * the port must answer on `127.0.0.1` only: the debug endpoint is
//!   verified against the system port listing **before** the WebSocket
//!   is used, and the run refuses on a wider binding;
//! * the driver walks the renderer's React state trees (the app's own
//!   view state, not its bridges — the store-first and mount-session
//!   surfaces are gated or single-claim, and this route decided against
//!   them) to read the conversation list and the loaded rows, and resolves
//!   the click target for a conversation to synthesize a UI click at its
//!   screen position;
//! * quit is by exact recorded pid (children listed by parent, never by
//!   name, never by pattern);
//! * nothing here could issue a request to any host that is not the
//!   loopback port named above: the HTTP and WebSocket URLs are built from
//!   `127.0.0.1:port` literals, and no credential store or vendor
//!   endpoint name appears anywhere in this file (pinned by a statics
//!   test).
//!
//! The view-state predicates are the ones the probe described, as
//! structure-agnostic walks so they survive minor layout shuffles; they
//! fail loudly on a real change instead of guessing, and a shape break is
//! reported with the app version. Developed against the app whose bundle
//! this preflight reads; unknown versions are allowed — loud on shape,
//! never guessing.
//!
//! The process side (`open`, the port listing, pid listing, kill) sits
//! behind the [`OsCommands`] trait: production shells out to the system
//! tools, tests drive a recording fake — no test of this build ever
//! opens, kills or quits anything on a real machine.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use super::{
    AppDriver, AppStatus, DriverError, RestoreOutcome, RowsSnapshot, UiConversation,
    APP_BOOT_WAIT_MS,
};

/// The application's name as the system opens it.
const APPLICATION_NAME: &str = "Grok Bot";

/// Where the shipped build looks for the app bundle, in order. The tilde
/// form expands against the environment at construction; tests inject a
/// sandbox path instead so nothing ever touches a real machine.
const BUNDLE_LOCATIONS: &[&str] = &["/Applications/Grok Bot.app", "~/Applications/Grok Bot.app"];

/// The only host forms the debug port may bind. Anything wider refuses.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "[::1]", "::1"];

/// The bounded wait, milliseconds, for a quitting app's exact recorded
/// pids to fully exit: an Electron main process ends some time after its
/// quit signal, and until it is fully gone it still holds the app's
/// single-instance lock, so any relaunch inside the window — the debug
/// one at the start of the run, the plain one at the close-out — dies
/// on it. The wait ends only on a confirmed exit; a timeout is a
/// reported failure, never a guessed release.
const QUIT_EXIT_WAIT_MS: u64 = 15_000;

/// Gap between exit-confirmation polls, milliseconds.
const QUIT_EXIT_POLL_INTERVAL_MS: u64 = 200;

/// The bounded wait, milliseconds, for the owed plain relaunch to show
/// a running process.
const RELAUNCH_APPEAR_WAIT_MS: u64 = 10_000;

/// The settle window, milliseconds, the owed plain relaunch must stay
/// running through — wide enough to catch an instance that starts and
/// immediately dies on a lingering single-instance lock, without
/// holding the run open long enough to matter.
const RELAUNCH_SETTLE_WAIT_MS: u64 = 2_000;

/// Gap between relaunch-verification polls, milliseconds.
const RELAUNCH_VERIFY_POLL_INTERVAL_MS: u64 = 250;

// ---------------------------------------------------------------------------
// The process shell.
//

pub trait OsCommands {
    /// Run a command to completion, answering captured stdout (stderr in
    /// the error text).
    fn run(&self, program: &str, args: &[&str]) -> Result<String>;
    /// Start a detached command (`open`): fire and forget; errors of the
    /// command itself surface through the next status read.
    fn start(&self, program: &str, args: &[&str]) -> Result<()>;
    /// The pids whose command line contains the marker — the running app's
    /// exact executable path. Listing only; nothing kills by marker.
    fn pids_matching(&self, marker: &str) -> Result<Vec<u32>>;
    /// Terminate one exact pid.
    fn kill(&self, pid: u32) -> Result<()>;
    /// The port listing: one line per listener on the port, the local
    /// address embedded as a whitespace-separated token.
    fn port_listeners(&self, port: u16) -> Result<Vec<String>>;
    /// The children of one exact pid. The default asks the system listing
    /// tool; test fakes answer from their journal.
    fn pids_of_parent(&self, pid: u32) -> Vec<u32> {
        let pid = pid.to_string();
        self.run("pgrep", &["-P", &pid])
            .map(|out| {
                out.lines()
                    .filter_map(|line| line.trim().parse::<u32>().ok())
                    .collect()
            })
            // reason: a helper that no longer exists has no children to
            // quit; the parent's own quit below is the loud part
            .unwrap_or_default()
    }
    /// Whether one exact pid is still alive. Listing only; nothing
    /// signals here. A probe that cannot be made answers alive — a
    /// process this could not see must not be declared dead while it
    /// may still hold the app's single-instance lock.
    fn pid_alive(&self, pid: u32) -> bool;
}

/// The production shell: the system tools, nothing else.
pub struct SystemOs;

impl OsCommands for SystemOs {
    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("run {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn start(&self, program: &str, args: &[&str]) -> Result<()> {
        Command::new(program)
            .args(args)
            .spawn()
            .with_context(|| format!("start {program}"))?;
        Ok(())
    }

    fn pids_matching(&self, marker: &str) -> Result<Vec<u32>> {
        // No `-P` here: the marker is what matches, and only the caller's
        // exact-pid kills use the answer.
        let output = Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .context("list the app's processes")?;
        if !output.status.success() && !output.stdout.is_empty() {
            bail!("the process listing tool failed");
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect())
    }

    fn kill(&self, pid: u32) -> Result<()> {
        // One signal to exactly one pid. A process that ended on its own
        // between listing and signal is the goal state, not a failure.
        match Command::new("kill").arg(pid.to_string()).output() {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("No such process") {
                    Ok(())
                } else {
                    bail!(
                        "the app process {} could not be quit: {}",
                        pid,
                        stderr.trim()
                    );
                }
            }
            Err(error) => bail!("the app process {pid} could not be signalled: {error}"),
        }
    }

    fn port_listeners(&self, port: u16) -> Result<Vec<String>> {
        let output = Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .output()
            .context("inspect the debug port's listeners")?;
        if !output.status.success() && !output.stdout.is_empty() {
            bail!("the port listing tool failed on port {port}");
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with("COMMAND"))
            .map(|line| line.to_string())
            .collect())
    }

    fn pid_alive(&self, pid: u32) -> bool {
        // One existence probe, `-0`, to exactly one pid: no signal is
        // delivered. `/bin/kill` exists for this on macOS the way it
        // exists for the real signal above.
        match Command::new("kill").arg("-0").arg(pid.to_string()).output() {
            // reason: a liveness probe that cannot be run must not
            // answer dead — the close-out's relaunch would race a
            // process that was never seen exiting, and aliveness is
            // the answer the bounded wait fails loudly on
            Ok(output) => output.status.success(),
            Err(_) => true,
        }
    }
}

// ---------------------------------------------------------------------------
// The CDP scripting. Each expression carries a marker comment; the tests'
// mock endpoint routes on the same markers, which makes them part of the
// wire protocol rather than incidental comments. Every snippet reads the
// app's own React view state; the only synthesized input is an ordinary
// click.
//

const SCRIPT_LIST_CONVERSATIONS: &str = r#"(() => {
  /*cs:sidebar-list*/
  const out = new Set();
  const seen = new WeakSet();
  const uuid = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;
  function dig(value, depth) {
    if (value == null || typeof value !== 'object' || value instanceof Element || depth > 8) return;
    if (seen.has(value)) return; seen.add(value);
    if (Array.isArray(value)) { for (const v of value) dig(v, depth + 1); return; }
    const id = value.id ?? value.agentId ?? value.agent_id ?? value.agentUUID;
    if (typeof id === 'string' && uuid.test(id)) out.add(id);
    for (const v of Object.values(value)) dig(v, depth + 1);
  }
  for (const el of document.querySelectorAll('*')) {
    for (const key of Object.getOwnPropertyNames(el)) {
      if (!key.startsWith('__reactFiber$')) continue;
      const node = el[key];
      dig(node.memoizedProps, 0);
      dig(node.memoizedState, 0);
      let up = node.return;
      for (let i = 0; i < 3 && up; i++) { dig(up.memoizedProps, 0); up = up.return; }
    }
  }
  return { items: [...out].map(id => ({ id })) };
})()"#;

const SCRIPT_CURRENTLY_OPEN: &str = r#"(() => {
  /*cs:sidebar-current*/
  let openAgent = null;
  const seen = new WeakSet();
  const uuid = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;
  const active = new Set(['selected', 'isSelected', 'active', 'isActive', 'current', 'isCurrent']);
  function dig(value, depth) {
    if (openAgent !== null || value == null || typeof value !== 'object' || value instanceof Element || depth > 8) return;
    if (seen.has(value)) return; seen.add(value);
    if (Array.isArray(value)) { for (const v of value) dig(v, depth + 1); return; }
    const id = value.id ?? value.agentId;
    if (typeof id === 'string' && uuid.test(id)) {
      for (const key of active) if (value[key] === true || value[key] === 'true') { openAgent = id; return; }
    }
    for (const v of Object.values(value)) dig(v, depth + 1);
  }
  for (const el of document.querySelectorAll('*')) {
    for (const key of Object.getOwnPropertyNames(el)) {
      if (!key.startsWith('__reactFiber$')) continue;
      const node = el[key];
      dig(node.memoizedProps, 0);
      dig(node.memoizedState, 0);
    }
  }
  return { agent: openAgent };
})()"#;

fn script_locate_conversation(agent_id: &str) -> String {
    let needle = json!(agent_id).to_string();
    format!(
        r#"(() => {{
  /*cs:sidebar-locate*/
  const needle = {needle};
  let hitEl = null;
  const seen = new WeakSet();
  function has(value, depth) {{
    if (hitEl || value == null || depth > 6) return false;
    if (typeof value === 'string') return value === needle;
    if (typeof value !== 'object' || value instanceof Element) return false;
    if (seen.has(value)) return false; seen.add(value);
    if (Array.isArray(value)) {{
      for (const v of value) if (has(v, depth + 1)) return true;
      return false;
    }}
    for (const v of Object.values(value)) if (has(v, depth + 1)) return true;
    return false;
  }}
  for (const el of document.querySelectorAll('*')) {{
    if (hitEl) break;
    for (const key of Object.getOwnPropertyNames(el)) {{
      if (!key.startsWith('__reactFiber$') && !key.startsWith('__reactProps$')) continue;
      const node = el[key];
      const props = node && node.memoizedProps !== undefined ? node.memoizedProps : node;
      if (has(props, 0)) {{
        const rect = el.getBoundingClientRect();
        if (rect.width > 0 && rect.height > 0 && rect.x >= 0 && rect.y >= 0) hitEl = el;
      }}
    }}
  }}
  if (!hitEl) return {{ found: false }};
  const rect = hitEl.getBoundingClientRect();
  return {{ found: true, x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 }};
}})()"#
    )
}

const SCRIPT_READ_ROWS: &str = r#"(() => {
  /*cs:transcript-rows*/
  let best = null;
  const seen = new WeakSet();
  function rowLike(entry) {
    return entry != null && typeof entry === 'object'
      && Number.isInteger(entry.seq) && typeof entry.kind === 'string';
  }
  function dig(value, depth) {
    if (value == null || typeof value !== 'object' || value instanceof Element || depth > 8) return;
    if (seen.has(value)) return; seen.add(value);
    if (Array.isArray(value)) {
      if (value.length > 0 && value.every(rowLike) && (!best || value.length > best.length)) best = value.slice();
      for (const v of value) dig(v, depth + 1);
      return;
    }
    for (const v of Object.values(value)) dig(v, depth + 1);
  }
  for (const el of document.querySelectorAll('*')) {
    for (const key of Object.getOwnPropertyNames(el)) {
      if (!key.startsWith('__reactFiber$')) continue;
      const node = el[key];
      dig(node.memoizedProps, 0);
      dig(node.memoizedState, 0);
    }
  }
  return { rows: best || [] };
})()"#;

const SCRIPT_VIEW_SIZE: &str = r#"(() => {
  /*cs:view-size*/
  return { width: window.innerWidth, height: window.innerHeight };
})()"#;

// ---------------------------------------------------------------------------
// The CDP transport: HTTP target discovery and one WebSocket, both built
// from `127.0.0.1` literals — the only host strings this file ever
// constructs. No redirect is followed, so a listener that answers
// elsewhere cannot carry a request off-loopback.
//

fn http_targets(port: u16) -> Result<Vec<Value>> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .context("build the debug-endpoint HTTP client")?
        .get(&url)
        .send()
        .with_context(|| format!("query {url}"))?;
    let status = response.status();
    let body = response.text().with_context(|| format!("read {url}"))?;
    if !status.is_success() {
        bail!("the debug endpoint answered HTTP {status}");
    }
    serde_json::from_str(&body).with_context(|| format!("parse the target list from {url}"))
}

fn page_target_path(targets: &[Value]) -> Result<String> {
    let target = targets
        .iter()
        .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .with_context(|| "the debug endpoint lists no page target (the app's renderer)")?;
    let url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .with_context(|| "the page target carries no debug URL")?;
    // Only the path is kept from the endpoint's own answer; the host the
    // WebSocket dials is rebuilt as the loopback literal, never the one
    // the answer names.
    let path = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, path)| format!("/{path}"))
        .with_context(|| format!("the debug URL {url} has no path"))?;
    Ok(path)
}

struct CdpConnection {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl CdpConnection {
    fn eval(&mut self, expression: &str) -> Result<Value> {
        self.call_method(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
            }),
        )
    }

    fn dispatch_mouse(&mut self, params: Value) -> Result<()> {
        self.call_method("Input.dispatchMouseEvent", params)?;
        Ok(())
    }

    fn call_method(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send(tungstenite::Message::Text(frame.to_string().into()))
            .with_context(|| format!("send a {method} frame"))?;
        loop {
            let message = self
                .socket
                .read()
                .with_context(|| format!("read the {method} reply"))?;
            match message {
                tungstenite::Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)
                        .with_context(|| format!("parse the {method} reply"))?;
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            bail!("CDP {method} failed: {error}");
                        }
                        return Ok(value
                            .pointer("/result/result/value")
                            .cloned()
                            .unwrap_or(Value::Null));
                    }
                    // Everything else is an event this driver never
                    // subscribed to; skip it.
                }
                tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => continue,
                tungstenite::Message::Close(frame) => {
                    bail!("the debug connection closed: {frame:?}")
                }
                tungstenite::Message::Binary(_) | tungstenite::Message::Frame(_) => continue,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The driver itself.
//

pub struct CdpAppDriver {
    os: Arc<dyn OsCommands + Send + Sync>,
    clock: super::Clock,
    persistence_root: PathBuf,
    /// The app bundle locations this driver probes, expanded once at
    /// construction.
    bundle_candidates: Vec<PathBuf>,
    /// The pids the run found running before it quit anything — what a
    /// normal relaunch owes back to the user.
    running_before: Vec<u32>,
    relaunched_pid: Option<u32>,
    /// Any `open` with the debug flag happened: the close-out must not
    /// leave a debug-flagged instance on the machine even when the pid
    /// recording failed.
    relaunched: bool,
    debug_port: u16,
    quit_something: bool,
    connection: Option<CdpConnection>,
}

impl CdpAppDriver {
    /// The driver this build ships: the system tooling and the app in its
    /// documented bundle locations.
    pub fn new(clock: super::Clock, persistence_root: &Path) -> Self {
        Self::with_locations(
            Arc::new(SystemOs),
            clock,
            persistence_root,
            BUNDLE_LOCATIONS
                .iter()
                .map(|location| shellexpand_tilde(location))
                .collect(),
        )
    }

    /// Test construction: an injected process shell (a recording fake) and
    /// an injected persistence root, still probing the shipped bundle
    /// locations.
    pub fn with_os(
        os: Arc<dyn OsCommands + Send + Sync>,
        clock: super::Clock,
        persistence_root: &Path,
    ) -> Self {
        Self::with_locations(
            os,
            clock,
            persistence_root,
            BUNDLE_LOCATIONS
                .iter()
                .map(|location| shellexpand_tilde(location))
                .collect(),
        )
    }

    /// Test construction with injected bundle locations, so no test's
    /// install probe ever reads a real machine's filesystem.
    pub fn with_locations(
        os: Arc<dyn OsCommands + Send + Sync>,
        clock: super::Clock,
        persistence_root: &Path,
        bundle_candidates: Vec<PathBuf>,
    ) -> Self {
        Self {
            os,
            clock,
            persistence_root: persistence_root.to_path_buf(),
            bundle_candidates,
            running_before: Vec::new(),
            relaunched_pid: None,
            relaunched: false,
            debug_port: 0,
            quit_something: false,
            connection: None,
        }
    }

    fn bundle(&self) -> Option<PathBuf> {
        self.bundle_candidates.iter().find_map(|path| {
            (path.join("Contents").join("Info.plist").is_file()).then_some(path.clone())
        })
    }

    fn bundle_version(&self, bundle: &Path) -> Option<String> {
        let out = self
            .os
            .run(
                "/usr/libexec/PlistBuddy",
                &[
                    "-c",
                    "Print :CFBundleShortVersionString",
                    &bundle.join("Contents").join("Info.plist").to_string_lossy(),
                ],
            )
            .ok()?;
        let version = out.trim().to_string();
        (!version.is_empty()).then_some(version)
    }

    fn open_app(&self, args: &[&str]) -> Result<()> {
        let mut all: Vec<&str> = vec!["-a", APPLICATION_NAME, "--args"];
        all.extend_from_slice(args);
        self.os.start("open", &all)
    }

    fn running_pids(&self) -> Vec<u32> {
        let marker = "/Grok Bot.app/Contents/MacOS/Grok Bot";
        // reason: a listing that could not be made answers "none seen"
        // rather than a named failure — the relaunch's bounded waits fail
        // loudly when the quit it assumed was wrong, and no later step
        // ever kills by name anyway
        self.os.pids_matching(marker).unwrap_or_default()
    }

    /// Whether every exact recorded pid is fully gone, polled by
    /// `pid_alive` until the last one answers dead or the bounded wait
    /// ends. Used before both relaunch openings — the debug relaunch
    /// races the pre-run instance's single-instance lock exactly the way
    /// the plain restore relaunch races the debug instance's.
    fn wait_pids_gone(&self, recorded: &[u32], wait_ms: u64) -> bool {
        let start = self.clock.now_ms();
        loop {
            if !recorded.iter().any(|pid| self.os.pid_alive(*pid)) {
                return true;
            }
            if self.clock.now_ms().saturating_sub(start) > wait_ms {
                return false;
            }
            self.clock.sleep_ms(QUIT_EXIT_POLL_INTERVAL_MS);
        }
    }

    /// Whether every listener line on the port is a loopback address. No
    /// address token or a non-loopback host counts as not verified — the
    /// run refuses rather than connect.
    fn port_is_loopback_only(&self, port: u16) -> Result<bool> {
        let listeners = self.os.port_listeners(port)?;
        let port_tag = port.to_string();
        for line in listeners {
            let address_token = line.split_whitespace().find(|token| {
                let token = token.strip_suffix("(LISTEN)").unwrap_or(token).trim();
                match token.rsplit_once(':') {
                    Some((_, tail)) => tail == port_tag,
                    None => false,
                }
            });
            let Some(token) = address_token else {
                return Ok(false);
            };
            let token = token.strip_suffix("(LISTEN)").unwrap_or(token).trim();
            let Some((host, tail)) = token.rsplit_once(':') else {
                return Ok(false);
            };
            if tail != port_tag || !LOOPBACK_HOSTS.contains(&host) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn connected(&mut self) -> Result<&mut CdpConnection> {
        self.connection
            .as_mut()
            .ok_or_else(|| anyhow!("the debug connection is not open"))
    }
}

fn shellexpand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

impl AppDriver for CdpAppDriver {
    fn app_status(&mut self) -> Result<AppStatus> {
        let installed_version = match self.bundle() {
            Some(bundle) => self.bundle_version(&bundle),
            None => None,
        };
        Ok(AppStatus {
            installed_version,
            running_pids: self.running_pids(),
        })
    }

    fn relaunch_for_backfill(&mut self, port: u16) -> std::result::Result<u32, DriverError> {
        let fail = |what: String| DriverError::Fatal(anyhow!("the debug relaunch failed: {what}"));

        // Quit a running app by exact pid, children by parent alongside,
        // and record every exact pid the quit covers — then wait the quit
        // out: the exiting instance still holds the app's single-instance
        // lock until it is fully gone, and the debug relaunch this opens
        // must not start inside that window.
        let quit_pids = self.running_pids();
        if !quit_pids.is_empty() {
            self.running_before = quit_pids.clone();
            let mut quit_record: Vec<u32> = Vec::new();
            for pid in &quit_pids {
                for child in self.os.pids_of_parent(*pid) {
                    quit_record.push(child);
                    drop(self.os.kill(child));
                }
                quit_record.push(*pid);
                if let Err(error) = self.os.kill(*pid) {
                    return Err(fail(format!(
                        "the running app could not be quit: {error:#}"
                    )));
                }
            }
            self.quit_something = true;
            if !self.wait_pids_gone(&quit_record, QUIT_EXIT_WAIT_MS) {
                return Err(fail(
                    "the quit app did not fully exit within its bounded wait, so the \
                     debugging relaunch could not be started against its released \
                     single-instance lock"
                        .to_string(),
                ));
            }
        }
        self.debug_port = port;
        if let Err(error) = self.open_app(&[&format!("--remote-debugging-port={port}")]) {
            return Err(fail(error.to_string()));
        }
        self.relaunched = true;

        // The boot wait: the debug endpoint must come up and answer.
        let start = self.clock.now_ms();
        let targets = loop {
            match http_targets(port) {
                Ok(targets) => break targets,
                Err(_) => {
                    if self.clock.now_ms().saturating_sub(start) > APP_BOOT_WAIT_MS {
                        return Err(fail(format!(
                            "the debugging endpoint never answered on 127.0.0.1:{port} within \
                             {APP_BOOT_WAIT_MS} ms"
                        )));
                    }
                    self.clock.sleep_ms(500);
                }
            }
        };
        // Loopback verification before any WebSocket use: abort if wider.
        match self.port_is_loopback_only(port) {
            Ok(true) => {}
            Ok(false) => {
                return Err(fail(format!(
                    "the debug port {port} is bound wider than loopback; refusing to use it"
                )))
            }
            Err(error) => {
                return Err(fail(format!(
                    "the debug port could not be inspected: {error:#}"
                )))
            }
        }
        let path = match page_target_path(&targets) {
            Ok(path) => path,
            Err(error) => return Err(fail(format!("{error:#}"))),
        };
        let socket_url = format!("ws://127.0.0.1:{port}{path}");
        let connection = tungstenite::connect(&socket_url)
            .with_context(|| format!("connect {socket_url}"))
            .map(|(socket, _)| CdpConnection { socket, next_id: 0 })
            .map_err(|error| fail(error.to_string()))?;
        self.connection = Some(connection);

        // The relaunched main pid: whichever process appeared that was not
        // running before — recorded so the close-out quits exactly it.
        let start = self.clock.now_ms();
        loop {
            let fresh: Vec<u32> = self
                .running_pids()
                .into_iter()
                .filter(|pid| !quit_pids.contains(pid))
                .collect();
            if let Some(pid) = fresh.first() {
                self.relaunched_pid = Some(*pid);
                return Ok(*pid);
            }
            if self.clock.now_ms().saturating_sub(start) > 5_000 {
                return Err(fail(
                    "the relaunched app did not come up within the wait".to_string(),
                ));
            }
            self.clock.sleep_ms(250);
        }
    }

    fn list_conversations(&mut self) -> std::result::Result<Vec<UiConversation>, DriverError> {
        let shape = |what: String| DriverError::ShapeBreak(what);
        let answer = self
            .connected()
            .and_then(|connection| connection.eval(SCRIPT_LIST_CONVERSATIONS))
            .map_err(|error| shape(format!("the sidebar list could not be read: {error:#}")))?;
        let items = answer
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| shape("the sidebar list answer carries no items".into()))?;
        let mut conversations = Vec::new();
        for item in items {
            let agent_id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| shape("a sidebar item carries no id".into()))?
                .to_string();
            conversations.push(UiConversation { agent_id });
        }
        Ok(conversations)
    }

    fn currently_open(&mut self) -> std::result::Result<Option<String>, DriverError> {
        let shape = |what: String| DriverError::ShapeBreak(what);
        let answer = self
            .connected()
            .and_then(|connection| connection.eval(SCRIPT_CURRENTLY_OPEN))
            .map_err(|error| {
                shape(format!(
                    "the open conversation could not be read: {error:#}"
                ))
            })?;
        Ok(answer
            .get("agent")
            .and_then(Value::as_str)
            .map(|agent| agent.to_string()))
    }

    fn open_conversation(&mut self, agent_id: &str) -> std::result::Result<(), DriverError> {
        let shape = |what: String| DriverError::ShapeBreak(what);
        let located = self
            .connected()
            .and_then(|connection| connection.eval(&script_locate_conversation(agent_id)))
            .map_err(|error| {
                shape(format!(
                    "the conversation to open could not be located: {error:#}"
                ))
            })?;
        let (x, y) = match (
            located.get("found").and_then(Value::as_bool),
            located.get("x").and_then(Value::as_f64),
            located.get("y").and_then(Value::as_f64),
        ) {
            (Some(true), Some(x), Some(y)) => (x, y),
            _ => {
                return Err(shape(format!(
                    "no on-screen element names the conversation {agent_id}"
                )))
            }
        };
        for kind in ["mousePressed", "mouseReleased"] {
            let params = json!({
                "type": kind,
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1,
            });
            self.connected()
                .and_then(|connection| connection.dispatch_mouse(params))
                .map_err(|error| {
                    shape(format!(
                        "the conversation click could not be sent: {error:#}"
                    ))
                })?;
        }
        Ok(())
    }

    fn read_rows(&mut self) -> std::result::Result<RowsSnapshot, DriverError> {
        let shape = |what: String| DriverError::ShapeBreak(what);
        let answer = self
            .connected()
            .and_then(|connection| connection.eval(SCRIPT_READ_ROWS))
            .map_err(|error| shape(format!("the loaded rows could not be read: {error:#}")))?;
        let rows = answer
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| shape("the loaded rows answer carries no rows".into()))?;
        Ok(RowsSnapshot { rows })
    }

    fn nudge_older_pages(&mut self) -> std::result::Result<(), DriverError> {
        let shape = |what: String| DriverError::ShapeBreak(what);
        let size = self
            .connected()
            .and_then(|connection| connection.eval(SCRIPT_VIEW_SIZE))
            .map_err(|error| shape(format!("the view could not be measured: {error:#}")))?;
        let (width, height) = (
            size.get("width").and_then(Value::as_f64).unwrap_or(600.0),
            size.get("height").and_then(Value::as_f64).unwrap_or(600.0),
        );
        let params = json!({
            "type": "mouseWheel",
            "x": width / 2.0,
            "y": height / 2.0,
            "deltaX": 0,
            "deltaY": -600,
        });
        self.connected()
            .and_then(|connection| connection.dispatch_mouse(params))
            .map_err(|error| shape(format!("the up-scroll could not be sent: {error:#}")))?;
        Ok(())
    }

    fn restore_normal_launch(
        &mut self,
        previously_open: Option<&str>,
    ) -> std::result::Result<RestoreOutcome, DriverError> {
        if !self.quit_something && !self.relaunched && self.connection.is_none() {
            // The relaunch never got anywhere: there is nothing to restore,
            // and relaunching "normally" would start an app that was never
            // running — a state change masquerading as a restore.
            return Ok(RestoreOutcome::default());
        }
        let mut outcome = RestoreOutcome {
            attempted: true,
            debug_pid: self.relaunched_pid,
            debug_port: self.debug_port,
            ..Default::default()
        };
        // Best-effort re-selection of the previously-open conversation
        // while the debug instance still runs (the app remembers the last
        // open conversation across its own restart). Performed and
        // recorded, never claimed as observed on the relaunched plain
        // instance — a plain launch carries no debugging connection.
        if let Some(conversation) = previously_open {
            if let Some(connection) = self.connection.as_mut() {
                match connection.eval(&script_locate_conversation(conversation)) {
                    Ok(located) => {
                        let spot = (
                            located.get("found").and_then(Value::as_bool),
                            located.get("x").and_then(Value::as_f64),
                            located.get("y").and_then(Value::as_f64),
                        );
                        if let (Some(true), Some(x), Some(y)) = spot {
                            outcome.conversation_reselected = Some(true);
                            drop(connection.dispatch_mouse(json!({
                                "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1,
                            })));
                            drop(connection.dispatch_mouse(json!({
                                "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1,
                            })));
                        } else {
                            outcome.conversation_reselected = Some(false);
                        }
                    }
                    Err(_) => outcome.conversation_reselected = None,
                }
            }
        }
        // End the debug session first, so the app is not driven while it
        // is being quit. The exact pid when it was recorded, its children
        // by parent alongside — never a name, never a pattern. Every
        // exact pid the quit covers is recorded, because the wait below
        // judges the exit on exactly those.
        self.connection = None;
        let mut to_quit: Vec<u32> = Vec::new();
        if let Some(pid) = self.relaunched_pid {
            to_quit.push(pid);
        } else if self.relaunched {
            to_quit.extend(
                self.running_pids()
                    .into_iter()
                    .filter(|pid| !self.running_before.contains(pid)),
            );
        }
        let mut quit_record: Vec<u32> = Vec::new();
        for pid in &to_quit {
            for child in self.os.pids_of_parent(*pid) {
                quit_record.push(child);
                drop(self.os.kill(child));
            }
            quit_record.push(*pid);
            if let Err(error) = self.os.kill(*pid) {
                outcome.failure = Some(format!("the debug relaunch could not be quit: {error:#}"));
                return Ok(outcome);
            }
        }
        self.relaunched_pid = None;
        self.relaunched = false;
        // Wait the quit out before any relaunch: the exiting instance
        // holds the app's single-instance lock until it is fully gone,
        // and a plain relaunch started inside that window starts an
        // instance that dies on the lock and leaves the machine with no
        // app at all — the exact end state the close-out exists to
        // prevent. Full exit means every recorded pid answers dead and
        // the debugging port's socket has released. A wait that times
        // out is a reported failure, never a guessed release; the plain
        // relaunch below still runs as the best effort it always was.
        let start = self.clock.now_ms();
        loop {
            let any_alive = quit_record.iter().any(|pid| self.os.pid_alive(*pid));
            let port_free = self
                .os
                .port_listeners(self.debug_port)
                .map(|lines| lines.is_empty())
                // reason: a listing that cannot be made must be read as
                // "not released" — the relaunch would race a port this
                // could not see, and the timeout is the honest answer
                .unwrap_or(false);
            if !any_alive && port_free {
                break;
            }
            if self.clock.now_ms().saturating_sub(start) > QUIT_EXIT_WAIT_MS {
                outcome.failure.get_or_insert_with(|| {
                    "the quit debug launch did not fully exit within its bounded \
                     wait, so its single-instance lock could not be confirmed \
                     released"
                        .to_string()
                });
                break;
            }
            self.clock.sleep_ms(QUIT_EXIT_POLL_INTERVAL_MS);
        }
        if self.quit_something {
            if let Err(error) = self.open_app(&[]) {
                outcome.failure = Some(format!("the app could not be relaunched: {error:#}"));
                return Ok(outcome);
            }
            // Verify the relaunch: a fresh instance must appear, stay
            // running through a settle window (the single-instance race
            // shows up as an instance that starts and immediately dies)
            // and never carry the old debug port. A listing that answers
            // at all makes a "no fresh pid" a measurement; one that never
            // answers leaves it unobserved — and the close-out judges
            // the difference.
            let start = self.clock.now_ms();
            let mut listed_ok = false;
            let mut fresh: Vec<u32> = Vec::new();
            let appeared = loop {
                if let Ok(seen) = self
                    .os
                    .pids_matching("/Grok Bot.app/Contents/MacOS/Grok Bot")
                {
                    listed_ok = true;
                    let not_old: Vec<u32> = seen
                        .into_iter()
                        .filter(|pid| {
                            !quit_record.contains(pid) && !self.running_before.contains(pid)
                        })
                        .collect();
                    if !not_old.is_empty() {
                        fresh = not_old;
                        break true;
                    }
                }
                if self.clock.now_ms().saturating_sub(start) > RELAUNCH_APPEAR_WAIT_MS {
                    break false;
                }
                self.clock.sleep_ms(RELAUNCH_VERIFY_POLL_INTERVAL_MS);
            };
            if appeared {
                let settle_start = self.clock.now_ms();
                let settled = loop {
                    if !fresh.iter().any(|pid| self.os.pid_alive(*pid)) {
                        break false;
                    }
                    if self.clock.now_ms().saturating_sub(settle_start) > RELAUNCH_SETTLE_WAIT_MS {
                        break true;
                    }
                    self.clock.sleep_ms(RELAUNCH_VERIFY_POLL_INTERVAL_MS);
                };
                outcome.relaunch_running = Some(settled);
                if !settled {
                    outcome.failure.get_or_insert_with(|| {
                        "the relaunched app started and exited again within its \
                         settle window"
                            .to_string()
                    });
                }
            } else if listed_ok {
                outcome.relaunch_running = Some(false);
                outcome.failure.get_or_insert_with(|| {
                    "the relaunched app never showed a running process within its \
                         bounded wait"
                        .to_string()
                });
            } else {
                // reason: an unobservable process listing is an
                // unobservation, not a dead relaunch — the close-out
                // reports it as could-not-verify, never as a measurement
                outcome.relaunch_running = None;
            }
        }
        // Verify: nothing still listens on the debug port. `attempted`
        // keys the whole restore above; the port check is the last word.
        let start = self.clock.now_ms();
        let closed = loop {
            match self.os.port_listeners(self.debug_port) {
                Ok(lines) if lines.is_empty() => break true,
                Ok(_) => {
                    if self.clock.now_ms().saturating_sub(start) > 10_000 {
                        break false;
                    }
                    self.clock.sleep_ms(250);
                }
                Err(_) => break false,
            }
        };
        outcome.port_closed = Some(closed);
        if !closed {
            outcome
                .failure
                .get_or_insert_with(|| "the debugging port still listens".to_string());
        }
        self.quit_something = false;
        Ok(outcome)
    }

    fn sync_evidence_ms(&mut self) -> Result<Option<u64>> {
        // The newest replica blob's mtime under the persistence root, best
        // effort — through the existing reader's own path listing, read
        // only.
        let mut newest: Option<u64> = None;
        for agent in crate::grok_bot::read_from_app_support(&self.persistence_root)? {
            if let Ok(metadata) = std::fs::metadata(agent.source_path) {
                if let Ok(seen) = metadata.modified() {
                    let ms = seen
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis() as u64)
                        // reason: a timestamp before the epoch is not a
                        // time any real cache file carries, and dropping it
                        // keeps the answer a measurement of the files that
                        // do — both samples of the check come from here
                        .unwrap_or_default();
                    newest = Some(newest.map_or(ms, |earlier| earlier.max(ms)));
                }
            }
        }
        Ok(newest)
    }
}
