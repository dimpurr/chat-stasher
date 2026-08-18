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
//! Scope note: the stdio message loop itself is deliberately NOT implemented
//! here. `native-host --self-test` is the verifiable stub that proves the host
//! process starts and speaks one line of JSON; the framed loop is a later
//! ticket.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

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
/// One line, on stdout, then exit. It exists so that "the host process starts
/// and can speak" is verifiable *before* the framed stdio loop is written —
/// and so the next ticket has a fixed thing to regress against. Diagnostics
/// must never go to stdout: Chromium reads stdout as a `u32` length prefix, so
/// a stray log line is parsed as a multi-gigabyte frame and the pipe dies.
pub fn self_test_line(host_name: &str, version: &str) -> String {
    let value = serde_json::json!({
        "host": host_name,
        "version": version,
        "protocol": HOST_TYPE,
        "mode": "self-test",
        "message_loop": "not-implemented",
        "ok": true,
    });
    value.to_string()
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
