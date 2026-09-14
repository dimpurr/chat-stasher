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
//! Beyond `hello` and `deliver`, the host answers two **read-only queries**
//! (§6.4 `summary`, §6.5 `open_dashboard`) and writes nothing for either. They
//! live here because they are the same contract, and because the one thing
//! they share with delivery is the property that matters: an unknown is
//! reported as unknown. `summary` reads directory entries and shard mtimes —
//! it never opens a shard, never decrypts the repository and never touches the
//! network; the counts it reports are a scoped answer, not a scan of the
//! archive. `open_dashboard` starts this same binary as `ui` and hands the
//! per-launch URL to the calling extension only.
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
use std::io::{BufRead, Read, Write};
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
        // §6.4/§6.5 — the two parameterless read-only queries.
        Some("summary") => match no_parameters(&request) {
            Ok(()) => summary(echoed_id),
            Err(detail) => nack(echoed_id, NackKind::BadRequest, detail),
        },
        Some("open_dashboard") => match no_parameters(&request) {
            Ok(()) => open_dashboard(echoed_id),
            Err(detail) => nack(echoed_id, NackKind::BadRequest, detail),
        },
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
// Protocol v1 — the read-only queries (§6.4, §6.5)
// ===========================================================================

/// Window the `summary` message's recent counts cover (§6.4).
pub const SUMMARY_WINDOW_HOURS: u32 = 24;

/// How long `open_dashboard` waits for the dashboard to report its URL (§6.5).
///
/// The extension's own per-request budget is 60 s (§2) and the host must answer
/// inside it: a dashboard that has not bound its socket in 45 s is not coming
/// up. Starting the child is cheap; *reading the archive* is what takes time in
/// `ui`, and that happens before it binds.
pub const DASHBOARD_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// The two messages §6.4/§6.5 define take no parameters at all.
///
/// `hello` and `deliver` ignore request fields this document does not define
/// (§6), because refusing one would reject a real conversation for a reason no
/// document states. These two carry no item, so the safe direction is the other
/// one: an extra field is `bad-request`, which means a later version that wants
/// to give either message a parameter has to be a protocol change rather than a
/// field an older host silently ignores.
fn no_parameters(request: &serde_json::Value) -> std::result::Result<(), String> {
    let Some(fields) = request.as_object() else {
        return Err("request is not a JSON object".to_string());
    };
    for key in fields.keys() {
        if key != "protocol" && key != "type" {
            return Err(format!(
                "`{key}`: this message takes no parameters (nativehost-protocol.md §6.4/§6.5)"
            ));
        }
    }
    Ok(())
}

/// The wire shape of [`crate::json_out::CountState`].
///
/// Spelled out rather than produced with `serde_json::to_value`, which returns
/// a `Result` that this path would have to unwrap. `NotApplicable` cannot occur
/// here (this module asks no "does this machine have one" question), and if it
/// ever did, it is a non-count and is reported as an unknown rather than as
/// zero.
fn count_of(state: &crate::json_out::CountState) -> serde_json::Value {
    match state {
        crate::json_out::CountState::Known { count } => {
            serde_json::json!({"kind": "known", "count": count})
        }
        crate::json_out::CountState::Unknown { why }
        | crate::json_out::CountState::NotApplicable { why } => {
            serde_json::json!({"kind": "unknown", "why": why})
        }
    }
}

/// The wire shape of [`crate::json_out::TimeState`]. Same reasoning as
/// [`count_of`].
fn time_of(state: &crate::json_out::TimeState) -> serde_json::Value {
    match state {
        crate::json_out::TimeState::Known { unix } => {
            serde_json::json!({"kind": "known", "unix": unix})
        }
        crate::json_out::TimeState::Unknown { why } => {
            serde_json::json!({"kind": "unknown", "why": why})
        }
    }
}

/// One session directory, as the summary scanner saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSession {
    /// Leading dot-segment of the directory name (`sidecar::infer_harness`'s
    /// rule, reused rather than re-implemented). `None` = no usable prefix; the
    /// session is still counted.
    pub harness: Option<String>,
    /// Newest sealed shard's mtime, seconds since the epoch. `None` = no usable
    /// mtime, which makes the window count a lower bound.
    pub newest_mtime: Option<i64>,
}

/// What one read of `<stage>/sessions/` produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageScan {
    pub sessions: Vec<StageSession>,
    /// Parts that could not be listed at all (the sessions root, a machine
    /// partition, a session directory). Every session count is a lower bound
    /// while this is non-empty.
    pub unreadable: Vec<String>,
    /// Sessions whose newest shard had no usable mtime. Only the window count
    /// is a lower bound; the totals are still measured.
    pub time_unknown: Vec<String>,
}

/// Read the stage's `sessions/` tree — directory entries and shard mtimes only.
///
/// It never opens a shard: the count is of *sessions*, and the only per-session
/// fact it needs beyond the name is when the newest shard was written, which is
/// the file's own mtime. That is what keeps this a metadata read (§6.4).
///
/// Every reason names a *place* in the tree and an OS error, never a session id
/// and never a path below the stage: the machine partition is identified by
/// [`crate::store::machine_fingerprint`], the same digest `validate_stage_machines`
/// uses in its diagnostics.
fn scan_stage(stage: &Path) -> StageScan {
    let mut scan = StageScan::default();
    let sessions_root = stage.join(crate::store::SESSIONS_DIR);
    let machines = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        // A stage that has never received a bundle has no `sessions/` at all.
        // That is a measurement, not an unreadable answer: the host asked and
        // found nothing there.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return scan,
        Err(e) => {
            scan.unreadable
                .push(format!("the stage's `sessions` directory: {e}"));
            return scan;
        }
    };

    for machine in machines {
        let machine = match machine {
            Ok(entry) => entry,
            Err(e) => {
                scan.unreadable
                    .push(format!("a machine partition could not be listed: {e}"));
                continue;
            }
        };
        let Ok(kind) = machine.file_type() else {
            scan.unreadable
                .push("a machine partition has no readable file type".to_string());
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let fingerprint = crate::store::machine_fingerprint(&machine.file_name().to_string_lossy());
        let sessions = match fs::read_dir(machine.path()) {
            Ok(entries) => entries,
            Err(e) => {
                scan.unreadable.push(format!(
                    "machine partition {fingerprint} could not be listed: {e}"
                ));
                continue;
            }
        };
        for session in sessions {
            let session = match session {
                Ok(entry) => entry,
                Err(e) => {
                    scan.unreadable.push(format!(
                        "a session directory under machine {fingerprint} could not be listed: {e}"
                    ));
                    continue;
                }
            };
            let Ok(kind) = session.file_type() else {
                scan.unreadable.push(format!(
                    "a session directory under machine {fingerprint} has no readable file type"
                ));
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let dir = session.path();
            let shards = match crate::store::sealed_shard_entries(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    scan.unreadable.push(format!(
                        "a session directory under machine {fingerprint} could not be read: {e:#}"
                    ));
                    continue;
                }
            };
            if shards.is_empty() {
                // A directory that holds no sealed shard is not a session, and
                // must not make an empty stage look populated.
                continue;
            }
            let newest_mtime = shards
                .iter()
                .filter_map(|(_, path)| shard_mtime_secs(path))
                .max();
            let harness = crate::sidecar::infer_harness(&session.file_name().to_string_lossy());
            if newest_mtime.is_none() {
                scan.time_unknown.push(format!(
                    "a session directory under machine {fingerprint} has a shard with no readable mtime"
                ));
            }
            scan.sessions.push(StageSession {
                harness,
                newest_mtime,
            });
        }
    }
    scan
}

/// A shard's mtime in whole seconds. `None` = no usable mtime (unreadable, or a
/// file stamped before 1970), which is *unknown*, never zero.
fn shard_mtime_secs(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_secs()).ok()
}

/// "N parts could not be read, so this number is a lower bound, not a
/// measurement: <first>" — one sentence for every unknown count.
fn lower_bound_reason(what: &str, count: usize, first: &str) -> String {
    format!(
        "{count} part{} of the stage could not be read, so the {what} is a lower bound, not a \
         measurement; the first was: {first}",
        if count == 1 { "" } else { "s" }
    )
}

/// Per-harness accumulator, before the two counts are turned into wire values.
#[derive(Debug, Default)]
struct HarnessCount {
    total: u64,
    recent: u64,
    time_unknown: usize,
}

/// Build the `summary` response from a scan and the run record.
///
/// Pure, and takes the clock as a parameter: the three states each count can be
/// in are then assertable without a stage, a filesystem or a clock.
pub fn build_summary(
    scan: &StageScan,
    last_push: crate::json_out::TimeState,
    window_hours: u32,
    now_unix: std::result::Result<i64, String>,
) -> serde_json::Value {
    // The two ways a session count stops being a measurement, kept apart: a
    // partition nobody could list makes *both* counts lower bounds, while a
    // shard with no mtime leaves the totals exact and only the window short.
    let totals_why = scan
        .unreadable
        .first()
        .map(|first| lower_bound_reason("session count", scan.unreadable.len(), first));

    let cutoff = now_unix
        .as_ref()
        .ok()
        .map(|now| now - i64::from(window_hours) * 3600);

    let window_why = match (&totals_why, &now_unix, scan.time_unknown.first()) {
        (Some(why), _, _) => Some(why.clone()),
        (None, Err(why), _) => Some(why.clone()),
        (None, Ok(_), Some(first)) => Some(lower_bound_reason(
            "count of sessions written in the last 24 hours",
            scan.time_unknown.len(),
            first,
        )),
        (None, Ok(_), None) => None,
    };

    let mut buckets: std::collections::BTreeMap<Option<String>, HarnessCount> =
        std::collections::BTreeMap::new();
    for session in &scan.sessions {
        let bucket = buckets.entry(session.harness.clone()).or_default();
        bucket.total += 1;
        match session.newest_mtime {
            Some(at) if cutoff.is_some_and(|cutoff| at >= cutoff) => bucket.recent += 1,
            Some(_) => {}
            None => bucket.time_unknown += 1,
        }
    }

    // `by_harness` is empty — and only empty — when the total is unknown: the
    // buckets that were listed would be a lower bound presented as a split, and
    // an empty array here is read by the extension as "no split to show", not
    // as "no platforms".
    let by_harness: Vec<serde_json::Value> = match totals_why {
        Some(_) => Vec::new(),
        None => buckets
            .into_iter()
            .map(|(harness, bucket)| {
                let recent_state = match &window_why {
                    Some(why) => crate::json_out::CountState::unknown(why.clone()),
                    None => crate::json_out::CountState::known(bucket.recent),
                };
                serde_json::json!({
                    "harness": harness,
                    "total": count_of(&crate::json_out::CountState::known(bucket.total)),
                    "last_24h": count_of(&recent_state),
                })
            })
            .collect(),
    };

    let total_state = match &totals_why {
        Some(why) => crate::json_out::CountState::unknown(why.clone()),
        None => crate::json_out::CountState::known(scan.sessions.len() as u64),
    };
    let recent_total = scan
        .sessions
        .iter()
        .filter(|s| match (s.newest_mtime, cutoff) {
            (Some(at), Some(cutoff)) => at >= cutoff,
            _ => false,
        })
        .count();
    let window_state = match &window_why {
        Some(why) => crate::json_out::CountState::unknown(why.clone()),
        None => crate::json_out::CountState::known(recent_total as u64),
    };

    // The two statements are the same one, and this is where that is enforced
    // rather than asserted by a reader: `complete` is true exactly when nothing
    // in the answer is unknown.
    let complete = total_state_is_known(&total_state)
        && total_state_is_known(&window_state)
        && matches!(last_push, crate::json_out::TimeState::Known { .. });

    serde_json::json!({
        "protocol": PROTOCOL,
        "type": "summary",
        "ok": true,
        "window_hours": window_hours,
        "complete": complete,
        "sessions": {
            "total": count_of(&total_state),
            "last_24h": count_of(&window_state),
            "by_harness": by_harness,
        },
        "last_push": time_of(&last_push),
    })
}

fn total_state_is_known(state: &crate::json_out::CountState) -> bool {
    matches!(state, crate::json_out::CountState::Known { .. })
}

/// What the run record says about the last successful push (§6.4).
///
/// `run-state.json` holds the **most recent** pass and nothing earlier, so only
/// one of its four shapes establishes a push time; the other three are unknowns
/// with the reason that distinguishes them.
fn last_push_state() -> crate::json_out::TimeState {
    use crate::runstate::{self, RunOutcome, RunStateRead};
    match runstate::load(&crate::collect::default_state_dir()) {
        RunStateRead::Present(state) if state.outcome == RunOutcome::Completed => {
            crate::json_out::TimeState::known(state.finished_at_unix as i64)
        }
        RunStateRead::Present(state) => crate::json_out::TimeState::unknown(format!(
            "the most recent run-once pass ({}) finished at unix {} without creating a snapshot \
             with anything new in it, and the record holds only the most recent pass — so when \
             the last successful push happened is not recorded anywhere this host reads",
            runstate::outcome_word(state.outcome),
            state.finished_at_unix
        )),
        RunStateRead::Missing => crate::json_out::TimeState::unknown(
            "no run-once pass has ever been recorded on this machine, so there is no push time \
             to report; `chat-stasher status` says the same thing at length",
        ),
        RunStateRead::Unreadable(why) => crate::json_out::TimeState::unknown(format!(
            "a run record exists but is unreadable ({why}), so the last successful push cannot \
             be established"
        )),
    }
}

/// `summary` — how much is in the stage, in counts only (§6.4).
fn summary(request_id: Option<String>) -> serde_json::Value {
    let stage = match resolve_target() {
        HostTarget::Ready { stage, .. } => stage,
        HostTarget::Refused { kind, detail } => return nack(request_id, kind, detail),
    };
    let scan = scan_stage(&stage);
    let last_push = last_push_state();
    // A clock before 1970 cannot date anything, so the window is unknown rather
    // than computed against a wrong "now".
    let now = crate::manifest::now_unix_seconds().map_err(|e| format!("{e:#}"));
    build_summary(&scan, last_push, SUMMARY_WINDOW_HOURS, now)
}

/// The destination `open_dashboard` opens, or the reason it cannot (§6.5).
///
/// There is no default destination (ADR-013), so this never falls back to "the
/// only one declared must be it": the config has to say which, once, and the
/// extension that triggers the launch never gets to name one.
fn dashboard_destination(config: &Config) -> std::result::Result<String, String> {
    let declared = config
        .native_host
        .as_ref()
        .and_then(|section| section.destination.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(name) = declared else {
        return Err(format!(
            "no `[native_host] destination` in {}; add `destination = \"<name>\"` naming one of \
             your declared destinations — there is no default destination",
            crate::config::config_path().display()
        ));
    };
    if !config.destinations.contains_key(name) {
        let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
        names.sort_unstable();
        return Err(format!(
            "`[native_host] destination` names `{name}`, which {} does not declare; declared: {}",
            crate::config::config_path().display(),
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        ));
    }
    Ok(name.to_string())
}

/// The argv the dashboard is started with.
///
/// `[<this binary>, "ui", "--no-open", "--destination", <name>]` — the same
/// binary (`std::env::current_exe()`) and therefore the same build and config
/// as the host itself. `--no-open` because the *caller* opens the URL: the
/// extension puts it in a tab, and two browsers opening the same dashboard is
/// one too many.
pub fn dashboard_argv(binary: &Path, destination: &str) -> Vec<String> {
    vec![
        binary.to_string_lossy().into_owned(),
        "ui".to_string(),
        "--no-open".to_string(),
        "--destination".to_string(),
        destination.to_string(),
    ]
}

/// What `open_dashboard` needs from a started dashboard process.
///
/// A trait rather than a bare `std::process::Child`, so the wait, the timeout
/// and the exit-status mapping can be tested without a real repository, a real
/// socket or a real `chat-stasher ui`: the launch is the one part of this
/// message that cannot be asserted against a fixture stage.
pub trait DashboardProcess: Send {
    /// Take the child's stdout. `None` when it was already taken.
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    /// Stop the process, and say how it ended. Called only after the wait ran
    /// out, so that a dashboard nobody holds the URL of does not keep running.
    fn stop(&mut self) -> String;
    /// How it ended, once stdout closed on its own.
    fn ended(&mut self) -> String;
}

/// One sentence for how a dashboard process ended, from its exit status alone.
///
/// The child's stderr is discarded rather than relayed (§6.5): it can carry a
/// repository URL, which is a real hostname. The exit statuses are `ui`'s own
/// documented ones (`crates/chat-stasher/src/main.rs`, `cmd_ui`), so each
/// sentence says which of the CLI's outcomes happened.
fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(0) => "it exited successfully without ever listening".to_string(),
        Some(1) => {
            "it exited 1: it read the destination in full and there was nothing to show".to_string()
        }
        Some(2) => {
            "it exited 2 (usage error): the destination it was given is not usable as-is — most \
             often that destination has no `repo` set"
                .to_string()
        }
        Some(3) => {
            "it exited 3: it did not finish reading the archive, so it never came up (the key, \
             the repository or the config could not be read)"
                .to_string()
        }
        Some(code) => format!("it exited {code}"),
        None => "it was terminated by a signal".to_string(),
    }
}

/// The production [`DashboardProcess`]: a real child of this host.
struct RealDashboard {
    child: std::process::Child,
}

impl DashboardProcess for RealDashboard {
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>)
    }

    fn stop(&mut self) -> String {
        let killed = self.child.kill();
        let status = self.child.wait();
        let mut why = describe_wait(status);
        if let Err(e) = killed {
            why.push_str(&format!(" (it could not be stopped: {e})"));
        }
        why
    }

    fn ended(&mut self) -> String {
        describe_wait(self.child.wait())
    }
}

/// How a real child ended. A `wait` that failed is said as-is; a status that
/// arrived is handed to [`describe_status`].
fn describe_wait(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => describe_status(status),
        Err(e) => format!("its exit status could not be read: {e}"),
    }
}

/// Start the dashboard for real: same binary, stdout piped, everything else
/// detached.
///
/// stdin is `null` and stderr is `null` on purpose. stdin because a child that
/// shared the host's stdin could eat the browser's own pipe; stderr because the
/// host's stderr is a browser log and the child's diagnostics can name a
/// repository (see [`describe_status`]). stdout is the one stream that carries
/// something the host needs.
fn spawn_dashboard(argv: &[String]) -> std::io::Result<Box<dyn DashboardProcess>> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty argv"))?;
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(Box::new(RealDashboard { child }))
}

/// How a dashboard launch ended (§6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardStart {
    /// The child printed its URL. The line is printed only after the socket is
    /// bound, so this is the listen event; no second probe is attempted.
    Listening(String),
    /// The child ended before it printed a URL.
    Ended(String),
    /// No URL inside the budget; the child was stopped.
    TimedOut(String),
}

/// Pull the dashboard URL out of one line of the child's stdout.
///
/// Every other line of narration is ignored. The check is strict on purpose:
/// this function decides which bytes the host will hand to an extension as
/// "open this", so it accepts a loopback address with a 64-hex token and
/// nothing else — not `localhost`, not another host, not a URL with no token.
pub fn parse_dashboard_url(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = line.strip_prefix("http://127.0.0.1:")?;
    let (port, token) = rest.split_once("/?token=")?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A port that is not a number, or is 0, is not a port: `bind_ephemeral`
    // never assigns 0, so a line carrying it is not the line this host prints.
    match port.parse::<u16>() {
        Ok(parsed) if parsed != 0 => {}
        _ => return None,
    }
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(line.to_string())
}

/// How a dashboard process is started. A named alias because the signature is
/// in the production path *and* in the tests that stub it.
pub type DashboardSpawner = dyn FnMut(&[String]) -> std::io::Result<Box<dyn DashboardProcess>>;

/// Start a dashboard and wait until it listens (§6.5).
///
/// `spawn` is a parameter so the wait, the timeout and the exit mapping are
/// testable without a real `chat-stasher ui`; production passes
/// [`spawn_dashboard`].
///
/// The child is read on a worker thread and the URL arrives over a channel,
/// because `Read` has no timeout: a plain blocking read would let a wedged
/// child hold the host past the extension's own 60 s budget.
pub fn start_dashboard(
    argv: &[String],
    spawn: &mut DashboardSpawner,
    timeout: std::time::Duration,
) -> DashboardStart {
    let mut process = match spawn(argv) {
        Ok(process) => process,
        Err(e) => return DashboardStart::Ended(format!("it could not be started: {e}")),
    };
    let Some(stdout) = process.take_stdout() else {
        return DashboardStart::Ended("its output could not be read".to_string());
    };

    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    if sender.send(std::mem::take(&mut line)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return DashboardStart::TimedOut(process.stop());
        }
        match receiver.recv_timeout(remaining) {
            // A URL found: the child keeps running and the reader thread keeps
            // draining its stdout until the dashboard exits. Detaching is the
            // point — the host answers and goes away, and the dashboard stays.
            Ok(line) => {
                if let Some(url) = parse_dashboard_url(&line) {
                    return DashboardStart::Listening(url);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return DashboardStart::TimedOut(process.stop())
            }
            // stdout closed: the child ended (or its output is unreadable), and
            // this is the one path that can say *how*.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return DashboardStart::Ended(process.ended())
            }
        }
    }
}

/// The success response of `open_dashboard`, from the URL the child printed.
///
/// A named function rather than an inline `json!` because the committed schema
/// is the authority on this shape and a test validates *this* value against it
/// (`tests/nativehost_e2e_test.rs`) — the one response this host can produce
/// that no end-to-end test can reach without a real repository.
pub fn dashboard_response(url: &str) -> serde_json::Value {
    serde_json::json!({
        "protocol": PROTOCOL,
        "type": "open_dashboard",
        "ok": true,
        "url": url,
    })
}

/// `open_dashboard` — start the dashboard and hand back its URL (§6.5).
fn open_dashboard(request_id: Option<String>) -> serde_json::Value {
    let config = Config::load();
    // The same gate `hello` uses: a host that cannot name its stage is not
    // configured, and a dashboard launched from it would be a second,
    // differently-configured view of the same archive.
    if let HostTarget::Refused { kind, detail } = resolve_target() {
        return nack(request_id, kind, detail);
    }
    let destination = match dashboard_destination(&config) {
        Ok(name) => name,
        Err(detail) => return nack(request_id, NackKind::Config, detail),
    };
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            return nack(
                request_id,
                NackKind::Io,
                format!("cannot locate this binary to start the dashboard: {e}"),
            )
        }
    };

    let argv = dashboard_argv(&binary, &destination);
    match start_dashboard(&argv, &mut spawn_dashboard, DASHBOARD_START_TIMEOUT) {
        DashboardStart::Listening(url) => dashboard_response(&url),
        DashboardStart::Ended(why) => nack(
            request_id,
            NackKind::Io,
            format!(
                "the dashboard did not start: {why}. Run `chat-stasher ui --destination \
                 {destination}` yourself to see the full message"
            ),
        ),
        DashboardStart::TimedOut(why) => nack(
            request_id,
            NackKind::Io,
            format!(
                "the dashboard did not report a URL within {}s, so the process this host started \
                 was stopped and no browser tab will be opened: {why}",
                DASHBOARD_START_TIMEOUT.as_secs()
            ),
        ),
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
    use crate::json_out::TimeState;

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

    // -------------------------------------------------- §6.4 summary: counts

    /// One synthetic session. `harness` and `mtime` are the only two facts the
    /// scanner produces per session, so that is the whole fixture shape.
    fn session(harness: Option<&str>, mtime: Option<i64>) -> StageSession {
        StageSession {
            harness: harness.map(str::to_string),
            newest_mtime: mtime,
        }
    }

    fn count_of(value: &serde_json::Value) -> u64 {
        assert_eq!(value["kind"], "known", "expected a measurement: {value}");
        value["count"].as_u64().expect("a known count")
    }

    fn why_of(value: &serde_json::Value) -> String {
        assert_eq!(value["kind"], "unknown", "expected an unknown: {value}");
        value["why"]
            .as_str()
            .expect("an unknown carries a why")
            .to_string()
    }

    const NOW: i64 = 1_760_000_000;
    const HOUR: i64 = 3600;

    fn summary_of(scan: &StageScan, last_push: TimeState) -> serde_json::Value {
        build_summary(scan, last_push, SUMMARY_WINDOW_HOURS, Ok(NOW))
    }

    fn known_push() -> TimeState {
        TimeState::known(NOW - HOUR)
    }

    #[test]
    fn a_readable_stage_is_measured_and_says_so() {
        let scan = StageScan {
            sessions: vec![
                session(Some("deepseek"), Some(NOW - HOUR)),
                session(Some("deepseek"), Some(NOW - 3 * HOUR)),
                session(Some("claude-code"), Some(NOW - 30 * HOUR)),
                session(None, Some(NOW - 2 * HOUR)),
            ],
            unreadable: Vec::new(),
            time_unknown: Vec::new(),
        };
        let summary = summary_of(&scan, known_push());

        assert_eq!(summary["type"], "summary");
        assert_eq!(summary["ok"], true);
        assert_eq!(summary["window_hours"], 24);
        assert_eq!(summary["complete"], true);
        assert_eq!(count_of(&summary["sessions"]["total"]), 4);
        assert_eq!(count_of(&summary["sessions"]["last_24h"]), 3);
        assert_eq!(
            summary["last_push"]["unix"].as_i64(),
            Some(NOW - HOUR),
            "a recorded push is reported verbatim"
        );

        let buckets = summary["sessions"]["by_harness"].as_array().expect("array");
        assert_eq!(
            buckets.len(),
            3,
            "one bucket per harness, including the None one"
        );
        // The per-harness totals add up to the total: no bucket may be dropped
        // because its name was not usable.
        let sum: u64 = buckets
            .iter()
            .map(|bucket| count_of(&bucket["total"]))
            .sum();
        assert_eq!(sum, count_of(&summary["sessions"]["total"]));
        assert!(
            buckets.iter().any(|bucket| bucket["harness"].is_null()),
            "a session with no harness prefix keeps its own bucket: {summary}"
        );
    }

    /// The honesty rule at the heart of §6.4: a partition nobody could list
    /// must not turn into `count: 0`.
    #[test]
    fn an_unreadable_partition_is_unknown_never_zero() {
        let scan = StageScan {
            sessions: vec![session(Some("deepseek"), Some(NOW - HOUR))],
            unreadable: vec![
                "machine partition abc012 could not be listed: Permission denied".to_string(),
            ],
            time_unknown: Vec::new(),
        };
        let summary = summary_of(&scan, known_push());

        assert_eq!(summary["complete"], false);
        let total = why_of(&summary["sessions"]["total"]);
        assert!(total.contains("lower bound"), "{total}");
        assert!(total.contains("Permission denied"), "{total}");
        let window = why_of(&summary["sessions"]["last_24h"]);
        assert!(window.contains("lower bound"), "{window}");
        assert_eq!(
            summary["sessions"]["by_harness"].as_array().map(Vec::len),
            Some(0),
            "a split of a lower bound is not a measurement, so there is no split"
        );
    }

    #[test]
    fn a_shard_without_an_mtime_shortens_only_the_window() {
        let scan = StageScan {
            sessions: vec![
                session(Some("deepseek"), Some(NOW - HOUR)),
                session(Some("deepseek"), None),
            ],
            unreadable: Vec::new(),
            time_unknown: vec![
                "a session directory under machine abc012 has a shard with no \
                                readable mtime"
                    .to_string(),
            ],
        };
        let summary = summary_of(&scan, known_push());

        assert_eq!(summary["complete"], false);
        assert_eq!(
            count_of(&summary["sessions"]["total"]),
            2,
            "the session was found; only its time is unknown"
        );
        let window = why_of(&summary["sessions"]["last_24h"]);
        assert!(window.contains("no readable mtime"), "{window}");
        let buckets = summary["sessions"]["by_harness"].as_array().expect("array");
        assert_eq!(buckets.len(), 1);
        assert_eq!(count_of(&buckets[0]["total"]), 2);
        assert!(why_of(&buckets[0]["last_24h"]).contains("no readable mtime"));
    }

    #[test]
    fn the_window_opens_exactly_at_the_boundary() {
        let scan = StageScan {
            sessions: vec![
                session(Some("deepseek"), Some(NOW - 24 * HOUR)),
                session(Some("deepseek"), Some(NOW - 24 * HOUR - 1)),
            ],
            unreadable: Vec::new(),
            time_unknown: Vec::new(),
        };
        let summary = summary_of(&scan, known_push());
        assert_eq!(
            count_of(&summary["sessions"]["last_24h"]),
            1,
            "a shard written exactly 24 h ago is inside the window"
        );
    }

    #[test]
    fn an_unusable_clock_makes_the_window_unknown_and_not_empty() {
        let scan = StageScan {
            sessions: vec![session(Some("deepseek"), Some(NOW - HOUR))],
            unreadable: Vec::new(),
            time_unknown: Vec::new(),
        };
        let summary = build_summary(
            &scan,
            known_push(),
            SUMMARY_WINDOW_HOURS,
            Err("system clock is before the unix epoch".to_string()),
        );
        assert_eq!(count_of(&summary["sessions"]["total"]), 1);
        let why = why_of(&summary["sessions"]["last_24h"]);
        assert!(why.contains("epoch"), "{why}");
        assert_eq!(summary["complete"], false);
    }

    #[test]
    fn complete_is_true_exactly_when_nothing_is_unknown() {
        let known = StageScan {
            sessions: vec![session(Some("deepseek"), Some(NOW - HOUR))],
            unreadable: Vec::new(),
            time_unknown: Vec::new(),
        };
        assert_eq!(summary_of(&known, known_push())["complete"], true);
        // Same counts, but one part of the answer is a statement about a
        // missing record instead of a number.
        let no_push = summary_of(&known, TimeState::unknown("no run record"));
        assert_eq!(no_push["complete"], false);
        assert_eq!(count_of(&no_push["sessions"]["total"]), 1);
    }

    #[test]
    fn an_empty_stage_answers_zero_as_a_measurement() {
        let summary = summary_of(&StageScan::default(), known_push());
        assert_eq!(summary["complete"], true, "{summary}");
        assert_eq!(count_of(&summary["sessions"]["total"]), 0);
        assert_eq!(count_of(&summary["sessions"]["last_24h"]), 0);
        assert_eq!(
            summary["sessions"]["by_harness"].as_array().map(Vec::len),
            Some(0)
        );
    }

    // ------------------------------------------- §6.4/§6.5: no parameters

    #[test]
    fn the_read_only_queries_refuse_any_field_they_do_not_define() {
        assert!(no_parameters(&serde_json::json!({"protocol": 1, "type": "summary"})).is_ok());
        let extra = no_parameters(&serde_json::json!({
            "protocol": 1, "type": "open_dashboard", "destination": "elsewhere"
        }))
        .expect_err("an unexpected field must be refused, not ignored");
        assert!(extra.contains("destination"), "{extra}");

        // Through `respond`, which is what a browser actually reaches: the
        // refusal happens before any config or stage is consulted, so this
        // needs no fixture.
        let frame = serde_json::to_vec(&serde_json::json!({
            "protocol": 1, "type": "summary", "machine": "somewhere"
        }))
        .expect("frame");
        let response = respond(&frame);
        assert_eq!(response["type"], "nack");
        assert_eq!(response["kind"], "bad-request");
        assert_eq!(response["retryable"], false);
        assert!(response["detail"]
            .as_str()
            .expect("detail")
            .contains("machine"));
    }

    // ------------------------------------------------- §6.5 dashboard argv

    #[test]
    fn the_dashboard_is_started_with_no_open_and_a_named_destination() {
        let argv = dashboard_argv(Path::new("/usr/local/bin/chat-stasher"), "storagebox");
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/chat-stasher",
                "ui",
                "--no-open",
                "--destination",
                "storagebox"
            ]
        );
    }

    // -------------------------------------------------- §6.5 URL recognition

    fn good_url() -> String {
        format!("http://127.0.0.1:51234/?token={}", "ab".repeat(32))
    }

    #[test]
    fn the_printed_url_is_recognised() {
        let url = good_url();
        assert_eq!(parse_dashboard_url(&url).as_deref(), Some(url.as_str()));
        // The narration `ui` prints around it is not a URL, and neither is a
        // line that merely mentions the address.
        assert_eq!(
            parse_dashboard_url("[ui] bound        : 127.0.0.1:51234"),
            None
        );
        assert_eq!(parse_dashboard_url(""), None);
    }

    #[test]
    fn a_url_this_host_would_not_open_is_refused() {
        let cases = [
            // Another host: loopback is the whole reason the token is enough.
            format!("http://localhost:51234/?token={}", "ab".repeat(32)),
            format!(
                "http://127.0.0.1.evil.test:51234/?token={}",
                "ab".repeat(32)
            ),
            format!("https://127.0.0.1:51234/?token={}", "ab".repeat(32)),
            // No token at all, and a token that is not 64 lowercase hex.
            "http://127.0.0.1:51234/".to_string(),
            format!("http://127.0.0.1:51234/?token={}", "AB".repeat(32)),
            format!("http://127.0.0.1:51234/?token={}", "ab".repeat(31)),
            format!("http://127.0.0.1:51234/?token={}", "zz".repeat(32)),
            // A port that is not a port.
            format!("http://127.0.0.1:0/?token={}", "ab".repeat(32)),
            format!("http://127.0.0.1:abc/?token={}", "ab".repeat(32)),
        ];
        for case in cases {
            assert_eq!(parse_dashboard_url(&case), None, "accepted {case}");
        }
    }

    // --------------------------------------------- §6.5 the launch, stubbed

    /// A [`DashboardProcess`] the tests drive by hand: the "child" hands out
    /// whatever the test put in it, and records whether it was stopped.
    struct StubDashboard {
        stdout: Option<Box<dyn Read + Send>>,
        ended: String,
        stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl DashboardProcess for StubDashboard {
        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            self.stdout.take()
        }
        fn stop(&mut self) -> String {
            self.stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            "it was stopped by this host".to_string()
        }
        fn ended(&mut self) -> String {
            self.ended.clone()
        }
    }

    /// A spawner that hands the first (and only) call the given stdout. A test
    /// spawns once, so a second call would only hide a bug.
    fn stub_spawn(
        mut stdout: Option<Box<dyn Read + Send>>,
        ended: &str,
    ) -> (
        Box<DashboardSpawner>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stopped);
        let ended = ended.to_string();
        let mut used = false;
        let spawn = move |_argv: &[String]| {
            let stdout = if used {
                None
            } else {
                used = true;
                stdout.take()
            };
            Ok(Box::new(StubDashboard {
                stdout,
                ended: ended.clone(),
                stopped: std::sync::Arc::clone(&flag),
            }) as Box<dyn DashboardProcess>)
        };
        (Box::new(spawn), stopped)
    }

    /// A stdout that carries these bytes and then ends, like a child that
    /// prints and exits.
    fn text_reader(text: &str) -> Box<dyn Read + Send> {
        Box::new(std::io::Cursor::new(text.as_bytes().to_vec()))
    }

    fn argv() -> Vec<String> {
        dashboard_argv(Path::new("/bin/chat-stasher"), "laptop")
    }

    const BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

    #[test]
    fn a_child_that_prints_the_url_is_listening() {
        // Narration first, exactly as `ui` prints it, then the URL line.
        let stdout = format!(
            "[ui] sessions     : 3 in view / 3 in the archive\n[ui] warning      : do not share it.\n{}\n[ui] browser      : not opened (--no-open)\n",
            good_url()
        );
        let (mut spawn, stopped) = stub_spawn(Some(text_reader(&stdout)), "it exited 0");
        assert_eq!(
            start_dashboard(&argv(), &mut spawn, BUDGET),
            DashboardStart::Listening(good_url())
        );
        assert!(
            !stopped.load(std::sync::atomic::Ordering::SeqCst),
            "a dashboard that listened must be left running"
        );
    }

    #[test]
    fn a_child_that_ends_before_printing_a_url_reports_how_it_ended() {
        let (mut spawn, _) = stub_spawn(
            Some(text_reader("ui: the config declares 2 destination(s)\n")),
            "it exited 2 (usage error)",
        );
        assert_eq!(
            start_dashboard(&argv(), &mut spawn, BUDGET),
            DashboardStart::Ended("it exited 2 (usage error)".to_string())
        );
    }

    #[test]
    fn a_child_that_never_says_anything_is_stopped_at_the_deadline() {
        // A pipe whose write end the test keeps open: no bytes, no EOF, so the
        // read blocks exactly like a dashboard still reading its archive.
        let (reader, _writer) = std::io::pipe().expect("pipe");
        let (mut spawn, stopped) = stub_spawn(Some(Box::new(reader)), "unused");
        let outcome = start_dashboard(&argv(), &mut spawn, std::time::Duration::from_millis(50));
        assert!(
            matches!(outcome, DashboardStart::TimedOut(_)),
            "expected a timeout, got {outcome:?}"
        );
        assert!(
            stopped.load(std::sync::atomic::Ordering::SeqCst),
            "a dashboard nobody holds the URL of must not be left running"
        );
    }

    #[test]
    fn a_child_with_no_readable_output_is_reported_not_ignored() {
        let (mut spawn, _) = stub_spawn(None, "unused");
        assert_eq!(
            start_dashboard(&argv(), &mut spawn, BUDGET),
            DashboardStart::Ended("its output could not be read".to_string())
        );
    }

    #[test]
    fn a_child_that_cannot_be_started_at_all_is_reported() {
        let mut spawn = |_argv: &[String]| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            ))
        };
        let outcome = start_dashboard(&argv(), &mut spawn, BUDGET);
        match outcome {
            DashboardStart::Ended(why) => {
                assert!(why.contains("could not be started"), "{why}");
                assert!(why.contains("No such file"), "{why}");
            }
            other => panic!("expected Ended, got {other:?}"),
        }
    }

    #[test]
    fn the_dashboard_response_carries_the_url_and_nothing_else() {
        let response = dashboard_response(&good_url());
        assert_eq!(response["type"], "open_dashboard");
        assert_eq!(response["ok"], true);
        assert_eq!(response["url"], good_url());
        assert_eq!(
            response.as_object().map(|object| object.len()),
            Some(4),
            "the extension treats an extra field as a malformed response: {response}"
        );
    }
}
