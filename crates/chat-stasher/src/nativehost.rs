//! Native Messaging host registration (ADR-014 step 2): install / uninstall.
//!
//! ADR-014 moved the extension's primary write path off `chrome.downloads`,
//! because `saveAs: false` is overridden by the browser-level "ask where to
//! save each file before downloading" preference. At up to 200 backfilled
//! conversations a day that is 200 modal dialogs, and no extension-side flag
//! can suppress them. Native Messaging has no such dialog.
//!
//! The price is that the *browser* has to be told the host exists, and that
//! registration is five separate physical actions (install the binary, write a
//! host manifest, drop it into each browser's discovery directory or the
//! Windows registry, keep the binary executable, then connect). This module
//! collapses actions 2 and 3 into one command.
//!
//! Two properties are load-bearing and are asserted by
//! `tests/b98_nmhost_test.rs`:
//!
//! * **Nothing is written silently.** Every manifest path this command touches
//!   is printed, absolutely, by the caller in `main.rs`. "No error" is not the
//!   same claim as "a file landed".
//! * **Uninstall removes exactly what install wrote.** Discovery directories
//!   are shared with every other vendor on the machine — on the author's own
//!   machine `~/Library/Application Support/Google/Chrome/NativeMessagingHosts`
//!   holds nine other manifests — so removal is by exact file name, and the
//!   directory itself is never removed.
//!
//! Scope note: the file also carries the **protocol v1 host itself** — frame
//! codec, browser-launch detection and the one-request-one-response loop of
//! `contracts/nativehost-protocol.md`. That document names this file
//! explicitly ("Changing this file requires changing that document,
//! `apps/extension/lib/native-host.ts` and
//! `crates/chat-stasher/src/nativehost.rs` together"), so the host lives here
//! rather than in a module of its own.
//!
//! The framing rules that decide the shape of everything below:
//!
//! * **stdout carries the response frame and nothing else.** Every diagnostic
//!   goes to stderr; one stray byte on stdout is read by the browser as part
//!   of a `u32` length prefix and kills the pipe.
//! * **The length prefix is checked before anything is allocated.** A prefix
//!   over 64 MiB is answered `nack too-large` without reading the body — the
//!   point of the cap is not to politely refuse a large message, it is to
//!   refuse to *allocate* what a hostile or broken peer claims to be sending.
//! * **EOF inside a frame is not a message.** If the header or the body ends
//!   early there is nothing to answer and nothing is written; the process
//!   exits non-zero so a caller cannot read silence as a successful delivery.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::inbox;

/// The host name browsers look up, and the manifest file stem.
///
/// Chromium restricts this string to lowercase alphanumerics, `_` and `.`
/// (see [`validate_host_name`]), which is why it is `chat_stasher` and not
/// `chat-stasher`: a hyphen would make the manifest unloadable rather than
/// merely ugly. The reverse-DNS prefix follows every other manifest observed
/// in the wild (`com.1password.1password`, `com.openai.codexextension`, …).
pub const HOST_NAME: &str = "com.chat_stasher.host";

/// `description` field. Users see this nowhere; reviewers and future
/// maintainers see it when they open the JSON.
pub const HOST_DESCRIPTION: &str =
    "chat-stasher: archives browser conversations to your own local stage (stdio host)";

/// Pinned Chrome/Chromium extension id. Pinned by the `key` field in
/// `apps/extension/wxt.config.ts`, which is what makes it identical on every
/// machine and every unpacked build.
pub const CHROME_EXTENSION_ID: &str = "gihmdkkmmmkeiagjjiimacmgkdilofhi";

/// Pinned Firefox add-on id (`browser_specific_settings.gecko.id`).
pub const FIREFOX_EXTENSION_ID: &str = "chat-stasher@team.iopho.com";

/// `type` field. `stdio` is the only value either browser family accepts.
pub const HOST_TYPE: &str = "stdio";

/// Which OS layout to compute paths for. Selectable so the Linux and Windows
/// layouts can be shape-tested from a macOS checkout instead of being asserted
/// only by reading a vendor document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

impl Platform {
    /// The platform this binary is running on.
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Macos
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Windows => "windows",
        }
    }
}

/// Manifest dialect. The two families spell the allowlist differently and a
/// manifest carrying the wrong key is silently ignored, not rejected loudly —
/// hence one enum rather than one boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `allowed_origins: ["chrome-extension://<id>/"]` (trailing slash required).
    Chromium,
    /// `allowed_extensions: ["<gecko id>"]`.
    Gecko,
}

/// Browsers this command knows a discovery path for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Browser {
    Chrome,
    ChromeCanary,
    Chromium,
    Edge,
    Brave,
    Vivaldi,
    Firefox,
}

impl Browser {
    pub const ALL: [Browser; 7] = [
        Browser::Chrome,
        Browser::ChromeCanary,
        Browser::Chromium,
        Browser::Edge,
        Browser::Brave,
        Browser::Vivaldi,
        Browser::Firefox,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::ChromeCanary => "chrome-canary",
            Browser::Chromium => "chromium",
            Browser::Edge => "edge",
            Browser::Brave => "brave",
            Browser::Vivaldi => "vivaldi",
            Browser::Firefox => "firefox",
        }
    }

    pub fn family(self) -> Family {
        match self {
            Browser::Firefox => Family::Gecko,
            _ => Family::Chromium,
        }
    }
}

/// One resolved write target.
#[derive(Clone, Debug)]
pub struct Target {
    pub browser: Browser,
    /// Directory whose existence is taken as "this browser is installed".
    /// `None` = this platform has no cheap probe, so presence is unknown and
    /// the manifest is written regardless (Windows: the manifest lives in our
    /// own directory, the browser is found through the registry instead).
    pub profile_root: Option<PathBuf>,
    /// Directory the manifest goes in.
    pub dir: PathBuf,
    /// `<dir>/<HOST_NAME>.json`.
    pub manifest: PathBuf,
}

impl Target {
    /// Has this browser left its data directory behind? `None` profile roots
    /// answer `true`: unknown is not evidence of absence.
    pub fn browser_present(&self) -> bool {
        match &self.profile_root {
            Some(root) => root.is_dir(),
            None => true,
        }
    }
}

/// Default discovery root for a platform.
///
/// macOS: `~/Library/Application Support` — verified on the author's machine,
/// where seven browsers' `NativeMessagingHosts` directories already exist
/// under it. Linux: `$HOME` (each browser's relative path carries its own
/// `.config/…`). Windows: `%LOCALAPPDATA%`, which only holds the JSON — the
/// browser finds it through the registry.
pub fn default_root(platform: Platform, home: &Path) -> PathBuf {
    match platform {
        Platform::Macos => home.join("Library").join("Application Support"),
        Platform::Linux => home.to_path_buf(),
        Platform::Windows => match std::env::var_os("LOCALAPPDATA") {
            Some(local) if !local.is_empty() => PathBuf::from(local),
            _ => home.join("AppData").join("Local"),
        },
    }
}

/// Resolve one browser's target under `root`, or `None` when this build has no
/// path for that combination (rather than a guessed one).
pub fn target(
    platform: Platform,
    root: &Path,
    browser: Browser,
    host_name: &str,
) -> Option<Target> {
    let (profile_rel, nmh_rel): (Option<&str>, &str) = match (platform, browser) {
        // macOS: <root> = ~/Library/Application Support
        (Platform::Macos, Browser::Chrome) => (Some("Google/Chrome"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::ChromeCanary) => {
            (Some("Google/Chrome Canary"), "NativeMessagingHosts")
        }
        (Platform::Macos, Browser::Chromium) => (Some("Chromium"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::Edge) => (Some("Microsoft Edge"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::Brave) => {
            (Some("BraveSoftware/Brave-Browser"), "NativeMessagingHosts")
        }
        (Platform::Macos, Browser::Vivaldi) => (Some("Vivaldi"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::Firefox) => (Some("Mozilla"), "NativeMessagingHosts"),

        // Linux: <root> = $HOME. Note Firefox's directory is spelled in
        // lowercase-with-hyphens here and CamelCase on macOS.
        (Platform::Linux, Browser::Chrome) => {
            (Some(".config/google-chrome"), "NativeMessagingHosts")
        }
        (Platform::Linux, Browser::Chromium) => (Some(".config/chromium"), "NativeMessagingHosts"),
        (Platform::Linux, Browser::Edge) => {
            (Some(".config/microsoft-edge"), "NativeMessagingHosts")
        }
        (Platform::Linux, Browser::Brave) => (
            Some(".config/BraveSoftware/Brave-Browser"),
            "NativeMessagingHosts",
        ),
        (Platform::Linux, Browser::Vivaldi) => (Some(".config/vivaldi"), "NativeMessagingHosts"),
        (Platform::Linux, Browser::Firefox) => (Some(".mozilla"), "native-messaging-hosts"),
        // Chrome Canary is macOS/Windows only; Linux's unstable channel has a
        // different directory that this build has not verified.
        (Platform::Linux, Browser::ChromeCanary) => return None,

        // Windows: the JSON may live anywhere; the registry points at it.
        // One file per browser keeps the registry mapping 1:1 with the file,
        // so uninstall never has to reason about sharing.
        (Platform::Windows, _) => (None, "chat-stasher/NativeMessagingHosts"),
    };

    let dir = match profile_rel {
        // Per-browser discovery directory (macOS / Linux).
        Some(rel) => join_rel(&join_rel(root, rel), nmh_rel),
        // Our own directory, one subdirectory per browser (Windows).
        None => join_rel(root, nmh_rel).join(browser.id()),
    };
    Some(Target {
        browser,
        profile_root: profile_rel.map(|rel| join_rel(root, rel)),
        manifest: dir.join(format!("{host_name}.json")),
        dir,
    })
}

/// Join a `/`-separated relative template onto a root, one component at a
/// time, so the result uses the host OS separator.
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        out.push(part);
    }
    out
}

/// The manifest, in the field order the vendor examples use.
#[derive(serde::Serialize)]
struct Manifest<'a> {
    name: &'a str,
    description: &'a str,
    path: String,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_origins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_extensions: Option<Vec<String>>,
}

/// Render the manifest for one browser family.
///
/// `binary` is serialised through `serde_json`, which is the whole reason the
/// Windows backslash / space case is not a hazard here: escaping is not done
/// by hand.
pub fn render_manifest(
    family: Family,
    host_name: &str,
    binary: &Path,
    chrome_extension_id: &str,
    firefox_extension_id: &str,
) -> Result<String> {
    validate_host_name(host_name)?;
    if !binary.is_absolute() {
        bail!(
            "host binary path must be absolute, got {}",
            binary.display()
        );
    }
    let (allowed_origins, allowed_extensions) = match family {
        Family::Chromium => {
            validate_chromium_extension_id(chrome_extension_id)?;
            (
                Some(vec![format!("chrome-extension://{chrome_extension_id}/")]),
                None,
            )
        }
        Family::Gecko => {
            if firefox_extension_id.trim().is_empty() {
                bail!("firefox extension id must not be empty");
            }
            (None, Some(vec![firefox_extension_id.to_string()]))
        }
    };
    let manifest = Manifest {
        name: host_name,
        description: HOST_DESCRIPTION,
        path: binary.to_string_lossy().into_owned(),
        kind: HOST_TYPE,
        allowed_origins,
        allowed_extensions,
    };
    let mut json = serde_json::to_string_pretty(&manifest).context("render host manifest")?;
    json.push('\n');
    Ok(json)
}

/// Chromium's documented name grammar: lowercase alphanumerics, `_` and `.`,
/// no leading/trailing dot and no `..`. A name outside it does not produce a
/// warning; the host simply is never found.
pub fn validate_host_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("host name must not be empty");
    }
    for ch in name.chars() {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.';
        if !ok {
            bail!(
                "host name {name:?} contains {ch:?}: only lowercase a-z, 0-9, '_' and '.' are accepted"
            );
        }
    }
    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        bail!("host name {name:?} must not start or end with '.', or contain '..'");
    }
    Ok(())
}

/// Chromium extension ids are exactly 32 characters drawn from `a`–`p`.
/// A one-character mismatch is refused by the browser with "native messaging
/// host not found", so it is worth failing here instead.
pub fn validate_chromium_extension_id(id: &str) -> Result<()> {
    if id.len() != 32 || !id.chars().all(|c| matches!(c, 'a'..='p')) {
        bail!(
            "chromium extension id must be 32 characters in a-p, got {id:?} ({} chars)",
            id.len()
        );
    }
    Ok(())
}

/// What `install_one` did, per target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// No manifest was there before.
    Written,
    /// A manifest was there and its bytes already matched — the idempotent case.
    Unchanged,
    /// A manifest was there with different bytes (stale binary path, older id).
    Updated,
    /// The browser's data directory does not exist and it was not named
    /// explicitly, so nothing was written.
    SkippedBrowserAbsent,
}

impl InstallOutcome {
    pub fn id(self) -> &'static str {
        match self {
            InstallOutcome::Written => "wrote",
            InstallOutcome::Unchanged => "unchanged",
            InstallOutcome::Updated => "updated",
            InstallOutcome::SkippedBrowserAbsent => "skipped",
        }
    }
}

/// What `remove_one` did, per target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    Absent,
}

impl RemoveOutcome {
    pub fn id(self) -> &'static str {
        match self {
            RemoveOutcome::Removed => "removed",
            RemoveOutcome::Absent => "absent",
        }
    }
}

/// Write one manifest. `force` writes even when the browser looks absent.
///
/// Idempotence is by content: a second identical run reports `Unchanged` and
/// touches nothing, so there is never a second copy and never a rewritten
/// inode for a browser to race against.
pub fn install_one(target: &Target, content: &str, force: bool) -> Result<InstallOutcome> {
    if !force && !target.browser_present() {
        return Ok(InstallOutcome::SkippedBrowserAbsent);
    }
    let existing = match fs::read_to_string(&target.manifest) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("read {}", target.manifest.display()));
        }
    };
    if existing.as_deref() == Some(content) {
        return Ok(InstallOutcome::Unchanged);
    }
    fs::create_dir_all(&target.dir).with_context(|| format!("create {}", target.dir.display()))?;
    // tmp + rename: a browser reading the directory sees either the old
    // manifest or the new one, never a half-written one.
    let tmp = target.dir.join(format!(
        ".{}.{}.tmp",
        target
            .manifest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "host.json".to_string()),
        std::process::id()
    ));
    fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &target.manifest) {
        // Best effort, and said out loud when it fails: a stray dot-file left
        // in a browser's discovery directory is exactly the kind of debris
        // this command is supposed not to leave behind.
        if let Err(cleanup) = fs::remove_file(&tmp) {
            eprintln!(
                "install-native-host: could not remove temporary {}: {cleanup}",
                tmp.display()
            );
        }
        return Err(e).with_context(|| format!("install {}", target.manifest.display()));
    }
    Ok(if existing.is_some() {
        InstallOutcome::Updated
    } else {
        InstallOutcome::Written
    })
}

/// Remove exactly the one manifest file this command writes.
///
/// The directory is shared with every other vendor on the machine and is
/// therefore never removed, and no glob is ever used.
pub fn remove_one(target: &Target) -> Result<RemoveOutcome> {
    match fs::remove_file(&target.manifest) {
        Ok(()) => Ok(RemoveOutcome::Removed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoveOutcome::Absent),
        Err(e) => Err(e).with_context(|| format!("remove {}", target.manifest.display())),
    }
}

/// One Windows registry mutation, as an argv.
///
/// ⚠️ UNVERIFIED: no Windows machine was available for this ticket. The key
/// names come from the vendor documentation collected in the R23 spike; they
/// are shape-tested (`tests/b98_nmhost_test.rs`) but have never been executed
/// against a real registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl RegistryCommand {
    /// Printable form, for the "here is what I did / would do" line.
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(' ') {
                out.push('"');
                out.push_str(arg);
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

/// `HKCU` subkey for a browser, or `None` when this build has not got one.
pub fn registry_key(browser: Browser, host_name: &str) -> Option<String> {
    let vendor = match browser {
        Browser::Chrome => "Google\\Chrome",
        Browser::Chromium => "Chromium",
        Browser::Edge => "Microsoft\\Edge",
        Browser::Brave => "BraveSoftware\\Brave-Browser",
        Browser::Vivaldi => "Vivaldi",
        Browser::Firefox => "Mozilla",
        // Canary's Windows key is not documented in the material gathered for
        // ADR-014; guessing it would produce a silently dead registration.
        Browser::ChromeCanary => return None,
    };
    Some(format!(
        "HKCU\\Software\\{vendor}\\NativeMessagingHosts\\{host_name}"
    ))
}

/// The `reg.exe` argv for registering (or unregistering) one browser.
/// Per-user (`HKCU`) on purpose: `HKLM` would need an elevated process.
pub fn registry_command(
    browser: Browser,
    host_name: &str,
    manifest: &Path,
    uninstall: bool,
) -> Option<RegistryCommand> {
    let key = registry_key(browser, host_name)?;
    let args = if uninstall {
        vec!["delete".to_string(), key, "/f".to_string()]
    } else {
        vec![
            "add".to_string(),
            key,
            "/ve".to_string(),
            "/t".to_string(),
            "REG_SZ".to_string(),
            "/d".to_string(),
            manifest.to_string_lossy().into_owned(),
            "/f".to_string(),
        ]
    };
    Some(RegistryCommand {
        program: "reg.exe".to_string(),
        args,
    })
}

/// Run one registry command. Refuses to run anywhere but Windows rather than
/// pretending it succeeded.
pub fn apply_registry(command: &RegistryCommand) -> Result<()> {
    if !cfg!(target_os = "windows") {
        bail!("registry registration only applies on Windows");
    }
    let status = std::process::Command::new(&command.program)
        .args(&command.args)
        .status()
        .with_context(|| format!("run {}", command.display()))?;
    if !status.success() {
        bail!("{} exited {}", command.display(), status);
    }
    Ok(())
}

/// The single line `native-host --self-test` prints.
///
/// One line, on stdout, then exit. It proves "the host process starts and can
/// speak" without sending a request frame; `message_loop` states how the real
/// loop runs (one request per process, `nativehost-protocol.md` §2). Diagnostics
/// must never go to stdout: Chromium reads stdout as a `u32` length prefix, so
/// a stray log line is parsed as a multi-gigabyte frame and the pipe dies.
pub fn self_test_line(host_name: &str, version: &str) -> String {
    let value = serde_json::json!({
        "host": host_name,
        "version": version,
        "protocol": HOST_TYPE,
        "mode": "self-test",
        "message_loop": "one-request-per-process",
        "ok": true,
    });
    value.to_string()
}

// ===========================================================================
// Protocol v1 — frames
// ===========================================================================

/// The protocol version this build implements. `nativehost-protocol.md` §9:
/// a new version is a new document section, never an edit to this one.
pub const PROTOCOL: u32 = 1;

/// The versions a `nack protocol-version` advertises (§9).
pub const SUPPORTED_PROTOCOL: [u32; 1] = [PROTOCOL];

/// Largest request frame the host will accept (§2).
///
/// This is *our* cap, not the browser's: Chrome refuses to send more than
/// 64 MiB, Firefox allows 4 GB. A host that trusted the browser here would
/// allocate whatever a broken or hostile peer claimed.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

/// Largest response frame the browser will accept (§2). The length prefix
/// counts against it too, so the JSON body gets four bytes less.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// `detail` is truncated to this many bytes (§2).
pub const DETAIL_CAP: usize = 4096;

/// What the host reports for `hello.host_version`.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Marker appended to a `detail` that hit [`DETAIL_CAP`], so a reader can tell
/// a short message from the head of a long one.
const TRUNCATION_MARK: &str = " [truncated]";

/// What one read of a request frame produced.
#[derive(Debug, PartialEq, Eq)]
pub enum RequestFrame {
    /// A complete frame's body.
    Frame(Vec<u8>),
    /// The length prefix claimed more than [`MAX_REQUEST_BYTES`]. Nothing was
    /// allocated for the body and none of it was read.
    TooLarge { declared: u64 },
    /// EOF before a whole frame arrived — inside the header or inside the body.
    /// There is no message here to answer, and answering anyway would tell the
    /// caller something it cannot know.
    Truncated,
}

/// Read exactly one length-prefixed frame (§2).
///
/// `read` is allowed to return short reads — a pipe very often does — so both
/// the header and the body are filled in a loop. `read_exact` would do the
/// filling but reports a short read as `UnexpectedEof` with the partial bytes
/// lost, and the three outcomes here have to stay three outcomes.
pub fn read_request_frame<R: Read>(reader: &mut R) -> std::io::Result<RequestFrame> {
    let mut header = [0u8; 4];
    let mut filled = 0usize;
    while filled < header.len() {
        let read = reader.read(&mut header[filled..])?;
        if read == 0 {
            return Ok(RequestFrame::Truncated);
        }
        filled += read;
    }
    let declared = u64::from(u32::from_ne_bytes(header));
    if declared > MAX_REQUEST_BYTES {
        // Deliberately before the allocation below, and before any read of the
        // body: the whole point of the cap is to refuse to allocate what the
        // peer claims to be sending.
        return Ok(RequestFrame::TooLarge { declared });
    }
    let mut body = vec![0u8; declared as usize];
    let mut filled = 0usize;
    while filled < body.len() {
        let read = reader.read(&mut body[filled..])?;
        if read == 0 {
            return Ok(RequestFrame::Truncated);
        }
        filled += read;
    }
    Ok(RequestFrame::Frame(body))
}

/// Encode one response as a length-prefixed frame.
///
/// A response that would not fit in [`MAX_RESPONSE_BYTES`] is replaced by a
/// minimal `nack io` rather than being written truncated: a truncated frame is
/// not a shorter answer, it is an unparseable one, and the browser would report
/// it as a protocol failure with no way to tell it from a crash. The fields
/// that could grow are already bounded (`detail` by [`DETAIL_CAP`], so this is
/// a backstop, not a normal path).
pub fn encode_response_frame(response: &serde_json::Value) -> std::io::Result<Vec<u8>> {
    let mut json = to_frame_json(response)?;
    if 4 + json.len() > MAX_RESPONSE_BYTES {
        eprintln!(
            "native-host: response of {} bytes exceeds the {} byte frame cap; sending a nack instead",
            json.len(),
            MAX_RESPONSE_BYTES
        );
        json = to_frame_json(&oversize_nack())?;
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_ne_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Serialise one response object.
///
/// There is no fallback value here on purpose. A response is always built by
/// this module from `json!` literals — no non-string map keys, no non-finite
/// floats — so serialisation cannot fail; and if it somehow did, an empty body
/// would be a *zero-length frame*, which the browser reads as a protocol error
/// with no way to tell it from a crashed host. Reporting the failure and
/// writing nothing is the only answer that stays true.
fn to_frame_json(value: &serde_json::Value) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| std::io::Error::other(format!("serialise response: {e}")))
}

/// The response used when the real one does not fit. Short by construction.
fn oversize_nack() -> serde_json::Value {
    nack(
        None,
        NackKind::Io,
        "the response did not fit in one 1 MiB frame",
    )
}

// ===========================================================================
// Protocol v1 — messages
// ===========================================================================

/// Every way a request can be refused (§6.3). One variant per row of that
/// table; the `nack` spelling is [`NackKind::slug`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NackKind {
    ProtocolVersion,
    BadRequest,
    TooLarge,
    Integrity,
    InvalidBundle,
    Config,
    StageUnavailable,
    Io,
}

impl NackKind {
    /// The `kind` string, exactly as the schema enumerates it.
    pub fn slug(self) -> &'static str {
        match self {
            NackKind::ProtocolVersion => "protocol-version",
            NackKind::BadRequest => "bad-request",
            NackKind::TooLarge => "too-large",
            NackKind::Integrity => "integrity",
            NackKind::InvalidBundle => "invalid-bundle",
            NackKind::Config => "config",
            NackKind::StageUnavailable => "stage-unavailable",
            NackKind::Io => "io",
        }
    }

    /// The other half of the same table. Retryable means "the same bytes sent
    /// again may succeed"; it is a statement about the *cause*, not about how
    /// sad the message sounds.
    pub fn retryable(self) -> bool {
        match self {
            NackKind::Integrity | NackKind::StageUnavailable | NackKind::Io => true,
            NackKind::ProtocolVersion
            | NackKind::BadRequest
            | NackKind::TooLarge
            | NackKind::InvalidBundle
            | NackKind::Config => false,
        }
    }
}

/// Truncate a `detail` to [`DETAIL_CAP`] bytes without splitting a character.
fn cap_detail(detail: String) -> String {
    if detail.len() <= DETAIL_CAP {
        return detail;
    }
    let mut end = DETAIL_CAP - TRUNCATION_MARK.len();
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = detail[..end].to_string();
    out.push_str(TRUNCATION_MARK);
    out
}

/// Build one `nack` message.
fn nack(
    request_id: Option<String>,
    kind: NackKind,
    detail: impl Into<String>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "protocol": PROTOCOL,
        "type": "nack",
        "request_id": request_id,
        "kind": kind.slug(),
        "retryable": kind.retryable(),
        "detail": cap_detail(detail.into()),
    });
    if kind == NackKind::ProtocolVersion {
        value["supported"] = serde_json::json!(SUPPORTED_PROTOCOL);
    }
    value
}

/// `hello` request body. Only the fields the protocol names; unknown extras are
/// ignored rather than rejected, because §6.3's `bad-request` covers "missing
/// or malformed field" and says nothing about fields the contract never
/// mentions.
#[derive(Debug, Deserialize)]
struct DeliverRequest {
    request_id: String,
    name: String,
    payload: String,
    sha256: String,
}

/// `[A-Za-z0-9_-]{1,128}` (§6.2).
fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `[0-9a-f]{64}` (§6.2).
fn valid_sha256(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `^[a-z0-9]+-[^/\\]+\.json$` (§6.2).
///
/// Written by hand rather than with a regex crate: the grammar is four lines,
/// and a hand-written predicate can be read against the contract in one
/// sitting.
fn valid_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some(dash) = stem.find('-') else {
        return false;
    };
    let (head, tail) = stem.split_at(dash);
    let tail = &tail[1..];
    !head.is_empty()
        && head
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !tail.is_empty()
        && !tail.contains('/')
        && !tail.contains('\\')
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where the host would write, and as which machine — or why it cannot.
enum HostTarget {
    Ready { machine: String, stage: PathBuf },
    Refused { kind: NackKind, detail: String },
}

/// Resolve the config and the stage exactly as `ingest` resolves the machine
/// (§4), then check the stage without ever creating it.
///
/// The three ways this can fail are three different user actions, so they are
/// three different answers: no key at all (`config`, fix with the install
/// command), a config that could not be read or parsed (`config`, fix the
/// file), and a configured path that is not a directory
/// (`stage-unavailable`, recreate it or re-point the key).
fn resolve_target() -> HostTarget {
    let refused = |kind: NackKind, detail: String| HostTarget::Refused { kind, detail };

    let config = Config::load();
    if config.source.is_error_fallback() {
        return refused(
            NackKind::Config,
            format!(
                "the config file {} could not be read or parsed, so no stage is configured; \
                 fix it, then re-run `chat-stasher install-native-host --stage <path>`",
                crate::config::config_path().display()
            ),
        );
    }
    let declared = config
        .native_host
        .as_ref()
        .and_then(|section| section.stage.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(declared) = declared else {
        return refused(
            NackKind::Config,
            format!(
                "no `[native_host] stage` in {}; fix with: \
                 chat-stasher install-native-host --stage <path>",
                crate::config::config_path().display()
            ),
        );
    };
    let stage = PathBuf::from(declared);
    if !stage.is_absolute() {
        // The browser chooses the host's working directory, so a relative
        // value means a different stage depending on who started us.
        return refused(
            NackKind::Config,
            format!(
                "`[native_host] stage` is not an absolute path ({declared}); fix with: \
                 chat-stasher install-native-host --stage <absolute path>"
            ),
        );
    }

    let machine = match resolve_machine(&config) {
        Ok(machine) => machine,
        Err(detail) => return refused(NackKind::Config, detail),
    };

    // Read-only: existence and kind are both checked, and neither is created.
    match fs::metadata(&stage) {
        Ok(meta) if meta.is_dir() => HostTarget::Ready { machine, stage },
        Ok(_) => refused(
            NackKind::StageUnavailable,
            format!("stage {} exists but is not a directory", stage.display()),
        ),
        Err(e) => refused(
            NackKind::StageUnavailable,
            format!("stage {} is not usable: {e}", stage.display()),
        ),
    }
}

/// The machine id: an explicit config value wins, otherwise the persisted
/// 128-bit identity.
///
/// Unlike every CLI command, the host never *creates* the identity. The host
/// is started by the browser, not by the user's shell, so it does not see
/// environment variables a shell profile sets (such as `XDG_DATA_HOME`). If the
/// CLI's identity lives under such a variable, the host would find nothing at
/// the default path and mint a second identity, and every delivered shard
/// would land in a different machine's archive partition without a word. A
/// missing identity is therefore a `config` refusal that names the fix,
/// exactly like a missing stage.
fn resolve_machine(config: &Config) -> std::result::Result<String, String> {
    if let Some(machine) = config.machine.as_deref().filter(|m| !m.is_empty()) {
        return Ok(machine.to_string());
    }
    let path = crate::config::default_data_root().join("machine-identity");
    match crate::identity::load_identity_state(&path) {
        crate::identity::IdentityFileState::Loaded(id) => Ok(id.as_hex()),
        crate::identity::IdentityFileState::Missing => Err(format!(
            "no machine identity at {}; the host never creates one. Run any archiving command \
             once from your shell (for example `chat-stasher run-once ...`), or set `machine` \
             in {}. If your shell sets XDG_DATA_HOME, the browser does not see it: set \
             `machine` in the config instead",
            path.display(),
            crate::config::config_path().display()
        )),
        crate::identity::IdentityFileState::Unusable(error) => Err(format!(
            "machine identity file {} is present but unusable ({error:?}); do not delete it — \
             it is the key to this machine's archive partition",
            path.display()
        )),
    }
}

/// Answer one request frame. Never touches stdout; the caller frames the result.
pub fn respond(frame: &[u8]) -> serde_json::Value {
    let request: serde_json::Value = match serde_json::from_slice(frame) {
        Ok(value) => value,
        Err(e) => {
            return nack(
                None,
                NackKind::BadRequest,
                format!("request is not JSON: {e}"),
            )
        }
    };
    if !request.is_object() {
        return nack(
            None,
            NackKind::BadRequest,
            "request is not a JSON object".to_string(),
        );
    }

    // Only echoed when it already satisfies the schema, so a malformed value
    // cannot make the `nack` itself fail schema validation.
    let echoed_id = request
        .get("request_id")
        .and_then(|value| value.as_str())
        .filter(|id| valid_request_id(id))
        .map(str::to_string);

    match request.get("protocol").and_then(|value| value.as_u64()) {
        Some(version) if version == u64::from(PROTOCOL) => {}
        _ => {
            return nack(
                echoed_id,
                NackKind::ProtocolVersion,
                format!("this host implements protocol {PROTOCOL} only"),
            );
        }
    }

    match request.get("type").and_then(|value| value.as_str()) {
        Some("hello") => hello(echoed_id),
        Some("deliver") => deliver(request, echoed_id),
        Some(other) => nack(
            echoed_id,
            NackKind::BadRequest,
            format!("unknown message type {other:?}"),
        ),
        None => nack(
            echoed_id,
            NackKind::BadRequest,
            "request has no `type`".to_string(),
        ),
    }
}

/// `hello` — is the host there, and where does it write?
///
/// It resolves the config, the machine and the stage exactly as `deliver` does,
/// so an `ok` here is a statement about the next `deliver`, not a heartbeat.
fn hello(request_id: Option<String>) -> serde_json::Value {
    match resolve_target() {
        // A `hello` request carries no `request_id` in the schema, so the
        // refusal carries `null` — the same answer as an unreadable one.
        HostTarget::Refused { kind, detail } => nack(request_id, kind, detail),
        HostTarget::Ready { machine, stage } => serde_json::json!({
            "protocol": PROTOCOL,
            "type": "hello",
            "ok": true,
            "host_version": HOST_VERSION,
            "machine": machine,
            // Verbatim from the config: canonicalising would resolve symlinks
            // and report a path the user never wrote.
            "stage": stage.to_string_lossy(),
        }),
    }
}

/// `deliver` — archive one bundle.
fn deliver(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let parsed: DeliverRequest = match serde_json::from_value(request) {
        Ok(parsed) => parsed,
        Err(e) => return nack(request_id, NackKind::BadRequest, e.to_string()),
    };

    if !valid_request_id(&parsed.request_id) {
        return nack(request_id, NackKind::BadRequest, "malformed `request_id`");
    }
    if !valid_name(&parsed.name) {
        return nack(request_id, NackKind::BadRequest, "malformed `name`");
    }
    if !valid_sha256(&parsed.sha256) {
        return nack(request_id, NackKind::BadRequest, "malformed `sha256`");
    }
    let request_id = Some(parsed.request_id.clone());

    // Recomputed over the UTF-8 bytes of the *decoded* payload, which is what
    // makes the content hash identical across every channel.
    let payload_sha = sha256_hex(parsed.payload.as_bytes());
    if payload_sha != parsed.sha256 {
        return nack(
            request_id,
            NackKind::Integrity,
            "sha256 does not match the payload bytes",
        );
    }

    let (machine, stage) = match resolve_target() {
        HostTarget::Ready { machine, stage } => (machine, stage),
        HostTarget::Refused { kind, detail } => return nack(request_id, kind, detail),
    };

    // `invalid-bundle` is the answer for a payload this channel cannot archive.
    // `ingest` would keep such bytes as a raw-only record, because an inbox
    // file is the user's own drop box; a delivery is machine-fed, and the
    // protocol has a word for refusing it.
    if let Err(detail) = inbox::check_bundle(parsed.payload.as_bytes()) {
        return nack(request_id, NackKind::InvalidBundle, detail);
    }

    match inbox::seal_payload(
        &parsed.name,
        parsed.payload.as_bytes(),
        &stage,
        &machine,
        crate::store::DEFAULT_SHARD_BUCKET_CAP,
    ) {
        Ok(inbox::SealOutcome::Stored(consumed)) => serde_json::json!({
            "protocol": PROTOCOL,
            "type": "ack",
            "request_id": parsed.request_id,
            "status": "stored",
            "sha256": parsed.sha256,
            "shard": consumed.shard,
        }),
        Ok(inbox::SealOutcome::Duplicate(existing)) => serde_json::json!({
            "protocol": PROTOCOL,
            "type": "ack",
            "request_id": parsed.request_id,
            "status": "duplicate",
            "sha256": parsed.sha256,
            "shard": existing.matched_shard,
        }),
        Err(inbox::SealError::Lock(e)) => {
            nack(request_id, NackKind::StageUnavailable, format!("{e:#}"))
        }
        Err(inbox::SealError::Other(e)) => nack(request_id, NackKind::Io, format!("{e:#}")),
    }
}

// ===========================================================================
// Protocol v1 — how the host is started
// ===========================================================================

/// What this process was started as (§3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// An ordinary command line: parse it with clap as usual.
    CommandLine,
    /// A browser started us as its Native Messaging host.
    Host,
    /// A browser started us for an extension this build does not serve. The
    /// origin is carried so the refusal can name it.
    Foreign { origin: String },
}

/// Recognise a browser launch from the raw process arguments (§3).
///
/// `argv[0]` is the program name and is skipped. The two shapes are the two
/// browser families and nothing else:
///
/// * Chromium passes the extension origin as the first argument, and on Windows
///   appends `--parent-window=<n>`; the argument after the origin is ignored
///   for exactly that reason.
/// * Firefox passes the path of the host manifest we registered (always
///   `*.json`) followed by the add-on id.
///
/// A `chrome-extension://` origin with any other id, and a Firefox-shaped
/// launch for any other add-on, are both [`Launch::Foreign`]: the process was
/// started to serve somebody, just not us.
pub fn detect_launch(argv: &[std::ffi::OsString]) -> Launch {
    let args: Vec<String> = argv
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let Some(first) = args.first() else {
        return Launch::CommandLine;
    };

    if let Some(rest) = first.strip_prefix("chrome-extension://") {
        let id = rest.split('/').next().unwrap_or("");
        return if id == CHROME_EXTENSION_ID {
            Launch::Host
        } else {
            Launch::Foreign {
                origin: first.clone(),
            }
        };
    }

    if first.ends_with(".json") {
        if let Some(addon) = args.get(1) {
            return if addon == FIREFOX_EXTENSION_ID {
                Launch::Host
            } else {
                Launch::Foreign {
                    origin: addon.clone(),
                }
            };
        }
    }

    Launch::CommandLine
}

// ===========================================================================
// Protocol v1 — the one-request loop
// ===========================================================================

/// What one turn of the host produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutcome {
    /// One response frame was written. Success — including when that response
    /// is a `nack`: the protocol was honoured, and `nack` is what it says.
    Answered,
    /// Nothing could be read at all. Nothing was written; the caller exits
    /// non-zero (§2).
    Unreadable,
}

/// Read one request, write one response (§2).
pub fn serve_one<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<HostOutcome> {
    let response = match read_request_frame(reader)? {
        RequestFrame::Frame(body) => respond(&body),
        RequestFrame::TooLarge { declared } => nack(
            None,
            NackKind::TooLarge,
            format!(
                "length prefix {declared} exceeds the {MAX_REQUEST_BYTES} byte request cap; the body was not read"
            ),
        ),
        RequestFrame::Truncated => return Ok(HostOutcome::Unreadable),
    };
    let frame = encode_response_frame(&response)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(HostOutcome::Answered)
}

/// Serve one request on the real stdin/stdout and report the process exit code.
///
/// This is the whole of host mode, shared by the browser launch and by the
/// `native-host` subcommand, so manual testing exercises the same code the
/// browser does.
pub fn serve_stdin() -> std::process::ExitCode {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match serve_one(&mut stdin.lock(), &mut stdout.lock()) {
        Ok(HostOutcome::Answered) => std::process::ExitCode::SUCCESS,
        Ok(HostOutcome::Unreadable) => {
            eprintln!(
                "native-host: EOF before a whole request frame arrived (header or body was \
                 truncated); nothing was written to stdout"
            );
            // 3 is this repository's "did not finish reading" code. The
            // contract only asks for non-zero; using the code that already
            // means "no conclusion can be drawn from the absence" keeps the
            // host inside the CLI's existing vocabulary.
            std::process::ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("native-host: cannot write the response frame: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_name_grammar_rejects_hyphen_and_uppercase() {
        assert!(validate_host_name(HOST_NAME).is_ok());
        // The reason the host is not called `com.chat-stasher.host`.
        assert!(validate_host_name("com.chat-stasher.host").is_err());
        assert!(validate_host_name("com.Chat_Stasher.host").is_err());
        assert!(validate_host_name(".leading").is_err());
        assert!(validate_host_name("trailing.").is_err());
        assert!(validate_host_name("double..dot").is_err());
    }

    #[test]
    fn pinned_chrome_id_is_a_wellformed_chromium_id() {
        assert!(validate_chromium_extension_id(CHROME_EXTENSION_ID).is_ok());
        assert!(validate_chromium_extension_id("tooshort").is_err());
        // `z` is outside a-p.
        assert!(validate_chromium_extension_id(&"z".repeat(32)).is_err());
    }
}
