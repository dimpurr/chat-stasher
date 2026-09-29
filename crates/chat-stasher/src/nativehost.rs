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
//!   holds other vendors' manifests — so removal is by exact file name, and the
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
//! Beyond `hello` and `deliver`, the host answers four **read-only queries**
//! (§6.4 `summary`, §6.5 `open_dashboard`, §6.8 `other_installs`, §6.9
//! `identity_state`). These queries do not mutate the stage or archive.
//! `summary` reads stage directory entries and shard
//! mtimes; it never opens a shard, decrypts the repository or touches the
//! network. `open_dashboard` starts this same binary as `ui` and hands the
//! per-launch URL to the calling extension only. `other_installs` runs the
//! existing archive overview, then returns one count without returning install
//! ids or per-platform rows. `identity_state` answers from the host's own
//! coordination state — whether two live writers have been observed sharing one
//! install id — and writes nothing, which is why a popup may ask it on every
//! open. Every failed read remains a failure, never a zero.
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
//!
//! # Path sources (read 2026-09-26)
//!
//! "An extension is not a singleton" — one user is N machines × M browsers ×
//! K profiles (`36-EXTENSION-TOPOLOGY.md` §1). Registration is therefore a
//! **per browser × per OS** fact, and decision **D5** of that document splits
//! the matrix into *supported and tested* (Chrome, Chromium, Edge, Brave, Arc)
//! and *best effort, marked unverified* (Chrome Beta / Canary, Opera, Vivaldi).
//! Every path below carries the source it came from, because a guessed
//! discovery path does not produce a visible error — it produces a browser that
//! silently never finds the host:
//!
//! * **S1** Chrome for Developers, *Native messaging* —
//!   <https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging>
//!   (macOS / Linux user paths, Windows `HKCU\SOFTWARE\Google\Chrome\…`, the
//!   32-bit-before-64-bit registry probe, and Chrome for Testing).
//! * **S2** Chromium, `docs/user_data_dir.md` —
//!   <https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md>
//!   (per-channel data directories on all three OSes; and the note that Linux's
//!   `~/.config` part can be overridden by `$CHROME_CONFIG_HOME` /
//!   `$XDG_CONFIG_HOME` — see the gap recorded in [`default_root`]).
//! * **S3** Chromium, `remoting/tools/register_local_nm_hosts.sh` —
//!   <https://chromium.googlesource.com/chromium/src/+/06e52a1425e72fe847d8a33a975bc9fdbe780ee6/remoting/tools/register_local_nm_hosts.sh>
//!   (Chromium writes into `~/.config/google-chrome{,-beta,-unstable}/NativeMessagingHosts`).
//! * **S4** Chromium, `native_process_launcher_win.cc` —
//!   <https://chromium.googlesource.com/chromium/src/+/ad587c3edba02a0c746651f6963e1b6e3763f1f7/chrome/browser/extensions/api/messaging/native_process_launcher_win.cc>
//!   (the Windows key is a **branding-level literal**, not a per-channel one).
//! * **S5** Microsoft Learn, *Native messaging* (Edge) —
//!   <https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging>
//!   (macOS `Microsoft Edge {Channel_Name}`, Linux `microsoft-edge`, Windows
//!   `SOFTWARE\Microsoft\Edge\…`, and Edge's documented fallback to Chromium's
//!   then Chrome's key).
//!
//! Sources that are **not** vendor documentation are marked inline as
//! `NO VENDOR DOC`, meaning no primary source was located. "Not found" is not
//! "does not exist": for Arc,
//! Brave, Opera and Vivaldi no primary source for the path was located, so the
//! path is carried from a third-party implementation and the browser's support
//! tier can never be better than [`Support::Unverified`] on its own evidence:
//!
//! * **S6** Arc, macOS `~/Library/Application Support/Arc/User Data/NativeMessagingHosts`
//!   — <https://github.com/keepassxreboot/keepassxc-browser/issues/1793>,
//!   <https://github.com/keepassxreboot/keepassxc-browser/issues/1955>,
//!   <https://github.com/keepassxreboot/keepassxc-browser/issues/2171>.
//!   Note the extra `User Data` component: Arc is the one Chromium fork here
//!   whose manifest directory is *not* directly under its application-data root.
//! * **S7** Opera — <https://forums.opera.com/topic/15735/porting-extension-from-chrome-macos-native-messaging>,
//!   <https://github.com/keepassxreboot/keepassxc/issues/2879>.
//! * **S8** Brave — <https://github.com/gopasspw/gopass-jsonapi/blob/db70c6919e598d08190c9acfe1993a5c969156e8/internal/jsonapi/manifest/setup_windows.go>.
//!   Brave's *policy* key `Software\Policies\BraveSoftware\Brave` is a different
//!   key and must not be confused with the manifest one.
//! * **S9** Vivaldi — <https://github.com/vergenzt/TabFS/blob/master/install.sh>,
//!   <https://github.com/AdguardTeam/AdguardForMac/issues/1152>.
//! * **S10** Arc, Windows registry key
//!   `HKCU\Software\ArcBrowser\Arc\NativeMessagingHosts` —
//!   <https://github.com/chauncygu/collection-claude-code-source-code/blob/main/original-source-code/src/utils/claudeInChrome/common.ts>
//!   (a deobfuscated mirror of Anthropic's Claude-in-Chrome registry table,
//!   read 2026-09-26; echoed across ten-plus independent mirrors of the same
//!   file). What makes a third-party table usable and not a guess: its sibling
//!   keys for Chrome, Brave, Chromium, Edge, Vivaldi and Opera match S1, S4, S5,
//!   S8 and S9 one for one, and the product it ships in installs this host at
//!   scale on real Windows machines. Still `NO VENDOR DOC`: no Arc vendor
//!   document for the key was located (as of 2026-09-26), and no live registry
//!   has confirmed it — see [`registry_subkey_of`].
//!
//! Two consequences are load-bearing and are asserted by
//! `tests/nativehost_browser_matrix_test.rs`:
//!
//! * **No guessed registry key.** S4 shows Chromium's own lookup uses one
//!   branding-level key; there is no per-channel key to copy. Chrome Beta and
//!   Chrome Canary therefore return `None` for Windows and
//!   `install-native-host` says *"no registry key known in this build"* rather
//!   than writing an entry nothing reads.
//! * **An unsupported pair is not an absent browser.** `target()` returning
//!   `None` means *this build does not look there*, which `doctor` reports as
//!   [`crate::doctor`]'s `NoDiscoveryPath` — never as `NotRegistered`.

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension as _;
use serde::Deserialize;
use std::ffi::OsStr;
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

/// Support tier for one browser on one OS, from decision **D5** of
/// `36-EXTENSION-TOPOLOGY.md` §5.
///
/// The tier is not decoration: it is what `doctor` D8, the setup wizard and the
/// support matrix report, so it has to be the *evidenced* claim rather than the
/// intended one. It is deliberately an `Option` at the call site — see
/// [`Browser::support`] — because "unverified" and "we do not look there at
/// all" are two different statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Support {
    /// D5's promised matrix: supported and tested, with a path from a vendor's
    /// own documentation — and, on Windows, a registry key, without which the
    /// manifest is on disk and unreachable.
    Supported,
    /// D5's best-effort tier (Chrome Beta / Canary, Opera, Vivaldi), or a pair
    /// whose path has no primary vendor source (`NO VENDOR DOC`, S6–S9),
    /// or Windows without a registry key. Registration is attempted and
    /// reported, and never promised.
    Unverified,
}

impl Support {
    /// The slug reported by `doctor --json` and printed by the wizard.
    pub fn id(self) -> &'static str {
        match self {
            Support::Supported => "supported",
            Support::Unverified => "unverified",
        }
    }
}

/// Browsers this command knows a discovery path for, in D5's tier order.
///
/// The order is the tier order rather than alphabetical on purpose: `Browser::ALL`
/// is what `doctor` D8 and `install-native-host` print, and a reader comparing
/// three machines' output should find the promised browsers grouped, with the
/// best-effort ones below them, rather than interleaved by spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Browser {
    // -- D5 "supported and tested" -----------------------------------------
    Chrome,
    Chromium,
    Edge,
    Brave,
    /// macOS only: Arc ships no Linux build, so on Linux this browser has no
    /// discovery path at all (see [`Browser::support`], which answers `None`
    /// there rather than pretending).
    Arc,
    // -- D5 "best effort, marked unverified" -------------------------------
    ChromeBeta,
    ChromeCanary,
    Opera,
    Vivaldi,
    // -- Gecko, outside D5's Chromium-family matrix ------------------------
    /// Carried as `Supported`: its paths come from Mozilla's own documentation
    /// (`~/.mozilla/native-messaging-hosts`, `~/Library/Application
    /// Support/Mozilla/NativeMessagingHosts`) and its registration predates
    /// D5. Not part of D5's Chromium matrix, and not covered by its wording.
    Firefox,
}

impl Browser {
    pub const ALL: [Browser; 10] = [
        Browser::Chrome,
        Browser::Chromium,
        Browser::Edge,
        Browser::Brave,
        Browser::Arc,
        Browser::ChromeBeta,
        Browser::ChromeCanary,
        Browser::Opera,
        Browser::Vivaldi,
        Browser::Firefox,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Edge => "edge",
            Browser::Brave => "brave",
            Browser::Arc => "arc",
            Browser::ChromeBeta => "chrome-beta",
            Browser::ChromeCanary => "chrome-canary",
            Browser::Opera => "opera",
            Browser::Vivaldi => "vivaldi",
            Browser::Firefox => "firefox",
        }
    }

    /// The browsers this build reports at [`Support::Supported`]: D5's
    /// "supported and tested" list, plus Firefox.
    ///
    /// Firefox is not in D5's matrix — that decision is about the Chromium
    /// family — but it is not a best-effort browser either: its two manifest
    /// directories come from Mozilla's own documentation, its registration
    /// predates D5, and demoting it to `unverified` would tell users of a
    /// working browser not to trust it.
    ///
    /// Being in this list is necessary but not sufficient for
    /// [`Support::Supported`]: a pair also has to have somewhere to be
    /// registered on this OS.
    fn in_supported_matrix(self) -> bool {
        matches!(
            self,
            Browser::Chrome
                | Browser::Chromium
                | Browser::Edge
                | Browser::Brave
                | Browser::Arc
                | Browser::Firefox
        )
    }

    /// The support tier for this browser on `platform`, or `None` when this
    /// build has no discovery path for the pair at all.
    ///
    /// `None` and `Some(Support::Unverified)` are different answers and must not
    /// be collapsed: `None` means *the pair is outside the matrix and nothing was
    /// looked at*, while `Unverified` means *we will try, and you should not
    /// count on it*. On Windows the tier also depends on the registry key,
    /// because there a manifest with no key is a file no browser reads —
    /// reporting that as `Supported` would be the exact false promise D5's
    /// wording exists to prevent.
    pub fn support(self, platform: Platform) -> Option<Support> {
        if !self.has_path(platform) {
            return None;
        }
        let windows = platform == Platform::Windows;
        if windows && registry_subkey_of(self).is_none() {
            // Registered nowhere the browser looks. Say so.
            return Some(Support::Unverified);
        }
        if !self.in_supported_matrix() {
            return Some(Support::Unverified);
        }
        // Brave and Arc are on D5's supported list, but neither has a vendor
        // document for its path (`NO VENDOR DOC`, S6/S8/S9). D5 is the
        // owner's decision and is implemented as written; §8 of the W204 report
        // records the divergence between that decision and the evidence.
        Some(Support::Supported)
    }

    /// Is there a discovery path for this pair? Mirrors [`target`]'s table, and
    /// `the_support_tier_and_the_path_table_agree` pins the two together so the
    /// answer cannot drift from the paths actually written.
    pub fn has_path(self, platform: Platform) -> bool {
        match (platform, self) {
            // Arc ships for macOS and Windows only.
            (Platform::Linux, Browser::Arc) => false,
            // Every other pair in the table has a path on every platform.
            _ => true,
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
    /// Is the browser's data directory here? `None` = this build has no cheap
    /// probe on this platform, so the answer is **unknown**.
    ///
    /// This is a statement about the *browser*, never about the extension: a data
    /// directory is left behind by an uninstall, shared by every profile, and
    /// says nothing about whether the extension is loaded in any of them. It is
    /// the tri-state the caller needs in order not to print a guess — see
    /// `doctor`'s `detected`, which carries these three values through to the
    /// user.
    pub fn detected(&self) -> Option<bool> {
        self.profile_root.as_ref().map(|root| root.is_dir())
    }

    /// Has this browser left its data directory behind? `None` profile roots
    /// answer `true`: unknown is not evidence of absence, so a caller deciding
    /// whether to write a manifest writes it.
    pub fn browser_present(&self) -> bool {
        self.detected().unwrap_or(true)
    }
}

/// Default discovery root for a platform, derived from `home` alone.
///
/// macOS: `~/Library/Application Support` — verified on the author's machine,
/// where **every one of the ten browsers in [`Browser::ALL`] already has its
/// `NativeMessagingHosts` directory** under it, Arc's included and at exactly
/// the `Arc/User Data/…` shape S6 describes. That is the one path in this table
/// with third-party evidence only, so the filesystem is the strongest check
/// available for it and it agrees. Linux: `$HOME` (each browser's relative path carries its own
/// `.config/…`). Windows: `<home>\AppData\Local`, which only holds the JSON —
/// the browser finds it through the registry.
///
/// **Pure on purpose.** The same `(platform, home)` gives the same answer on
/// every machine, so a caller that has a specific home in mind — a test with a
/// `tempfile` home, `--target-root`, a probe asked about a directory — gets an
/// answer about *that* home. On Windows the *machine's* own answer can differ
/// (a profile may redirect the `LocalAppData` known folder), but that is a fact
/// about the machine and not about the home, so it lives in [`machine_root`].
///
/// # Known gap: Linux `$XDG_CONFIG_HOME` / `$CHROME_CONFIG_HOME`
///
/// S2 records that on Linux the `~/.config` component of every Chromium data
/// directory can be moved by `$CHROME_CONFIG_HOME` (Chrome) or
/// `$XDG_CONFIG_HOME` (the rest). This build does not consult either, so on a
/// machine that sets one, both `install-native-host` and `doctor` look in
/// `$HOME/.config/…` while the browser looks elsewhere — and the answer they give
/// is `not registered` / `skipped` rather than unknown. That is a *wrong negative*
/// and it is recorded here rather than fixed, because a faithful fix cannot be a
/// branch on this function: Firefox's Linux directory is `$HOME/.mozilla`, which
/// no `.config` variable moves, so one root cannot move for all of them and a
/// half-move would silently relocate every Chromium manifest for exactly the
/// users who set the variable. Tracked as a gap in the W204 report (§8); the
/// Windows analogue of this problem *is* handled, by [`machine_root`].
pub fn default_root(platform: Platform, home: &Path) -> PathBuf {
    match platform {
        Platform::Macos => home.join("Library").join("Application Support"),
        Platform::Linux => home.to_path_buf(),
        Platform::Windows => home.join("AppData").join("Local"),
    }
}

/// The discovery root *this machine* uses: [`default_root`], except on Windows
/// where `%LOCALAPPDATA%` is the known-folder answer and wins when it is set.
/// A profile can redirect that folder away from `<home>\AppData\Local`, and the
/// browser then reads the file where the variable points.
///
/// This is the only place the environment is consulted. It is deliberately a
/// separate function rather than a branch inside [`default_root`]: installing
/// the host wants the machine's answer, while probing a named directory wants
/// that directory's answer, and a shared function could only give one of them.
pub fn machine_root(platform: Platform, home: &Path) -> PathBuf {
    machine_root_with(platform, home, std::env::var_os("LOCALAPPDATA").as_deref())
}

/// [`machine_root`] with the environment read passed in, so the rule — the
/// variable wins on Windows, and nowhere else — can be asserted without
/// mutating a process-global that every other test in the binary shares.
fn machine_root_with(platform: Platform, home: &Path, local_appdata: Option<&OsStr>) -> PathBuf {
    if platform == Platform::Windows {
        if let Some(local) = local_appdata.filter(|value| !value.is_empty()) {
            return PathBuf::from(local);
        }
    }
    default_root(platform, home)
}

/// Resolve one browser's target under `root`, or `None` when this build has no
/// path for that combination (rather than a guessed one).
///
/// # The three layouts
///
/// * **macOS / Linux** — `<root>/<data dir>/NativeMessagingHosts/<host>.json`.
///   `root` is `~/Library/Application Support` and `$HOME` respectively, and the
///   data directories are the sources cited in this file's header.
/// * **Windows** — the manifest may live anywhere and the *registry* points at
///   it, so it goes in our own directory, one subdirectory per browser, which
///   keeps the registry mapping 1:1 with the file and lets uninstall reason
///   about exactly one key and one file per browser.
///
/// # The `profile_rel` half is a probe, not a promise
///
/// `profile_rel` is what [`Target::detected`] stats. On Windows it is filled in
/// only for the browsers whose data directory a **primary** source names (S2);
/// for the rest it stays `None`, which means *presence unknown* — and unknown is
/// not absence, so those browsers are written for regardless.
pub fn target(
    platform: Platform,
    root: &Path,
    browser: Browser,
    host_name: &str,
) -> Option<Target> {
    let (profile_rel, nmh_rel): (Option<&str>, &str) = match (platform, browser) {
        // ---- macOS: <root> = ~/Library/Application Support ----------------
        // S1 for Chrome and Chromium; S2 for the Chrome channels; S5 for Edge.
        (Platform::Macos, Browser::Chrome) => (Some("Google/Chrome"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::ChromeBeta) => {
            (Some("Google/Chrome Beta"), "NativeMessagingHosts")
        }
        (Platform::Macos, Browser::ChromeCanary) => {
            (Some("Google/Chrome Canary"), "NativeMessagingHosts")
        }
        (Platform::Macos, Browser::Chromium) => (Some("Chromium"), "NativeMessagingHosts"),
        (Platform::Macos, Browser::Edge) => (Some("Microsoft Edge"), "NativeMessagingHosts"),
        // S6 — Arc is the one Chromium fork whose application-data root carries
        // an extra `User Data` component before `NativeMessagingHosts`. Dropping
        // it would place the manifest in a directory Arc does not read, with no
        // error anywhere: the extension would simply never find the host.
        (Platform::Macos, Browser::Arc) => (Some("Arc/User Data"), "NativeMessagingHosts"),
        // S8 — NO VENDOR DOC: Brave's application-data root.
        (Platform::Macos, Browser::Brave) => {
            (Some("BraveSoftware/Brave-Browser"), "NativeMessagingHosts")
        }
        // S9 — NO VENDOR DOC.
        (Platform::Macos, Browser::Vivaldi) => (Some("Vivaldi"), "NativeMessagingHosts"),
        // S7 — NO VENDOR DOC: Opera uses its bundle identifier here,
        // unlike every other browser in this table.
        (Platform::Macos, Browser::Opera) => {
            (Some("com.operasoftware.Opera"), "NativeMessagingHosts")
        }
        (Platform::Macos, Browser::Firefox) => (Some("Mozilla"), "NativeMessagingHosts"),

        // ---- Linux: <root> = $HOME ---------------------------------------
        // Note Firefox's directory is spelled in lowercase-with-hyphens here and
        // CamelCase on macOS. The `.config` component is S1/S3 for the Chromium
        // family and is deliberately *not* resolved through `$XDG_CONFIG_HOME`
        // here — see the gap recorded on [`default_root`].
        (Platform::Linux, Browser::Chrome) => {
            (Some(".config/google-chrome"), "NativeMessagingHosts")
        }
        (Platform::Linux, Browser::ChromeBeta) => {
            (Some(".config/google-chrome-beta"), "NativeMessagingHosts")
        }
        // S2/S3 name `google-chrome-canary` for Linux. It is carried as an
        // unverified entry rather than dropped: no Linux Chrome Canary build was
        // found to exist, so on a Linux machine this normally reports `skipped`
        // — which *shows* the absence instead of hiding it behind a missing row.
        (Platform::Linux, Browser::ChromeCanary) => {
            (Some(".config/google-chrome-canary"), "NativeMessagingHosts")
        }
        (Platform::Linux, Browser::Chromium) => (Some(".config/chromium"), "NativeMessagingHosts"),
        (Platform::Linux, Browser::Edge) => {
            (Some(".config/microsoft-edge"), "NativeMessagingHosts")
        }
        // S8 — NO VENDOR DOC.
        (Platform::Linux, Browser::Brave) => (
            Some(".config/BraveSoftware/Brave-Browser"),
            "NativeMessagingHosts",
        ),
        // S9 — NO VENDOR DOC.
        (Platform::Linux, Browser::Vivaldi) => (Some(".config/vivaldi"), "NativeMessagingHosts"),
        // S7 — NO VENDOR DOC.
        (Platform::Linux, Browser::Opera) => (Some(".config/opera"), "NativeMessagingHosts"),
        (Platform::Linux, Browser::Firefox) => (Some(".mozilla"), "native-messaging-hosts"),
        // Arc ships for macOS and Windows only; there is nothing to look for.
        (Platform::Linux, Browser::Arc) => return None,

        // ---- Windows: registry-keyed; manifest in our own directory -------
        // The data directories below are S2. The probe is the *parent* of
        // `…\User Data`, not `User Data` itself: a redirected profile directory
        // leaves the parent standing, and a probe that went one level deeper
        // would report a browser that is right there as absent.
        //
        // Edge's and Firefox's Windows data directories are not named by a
        // primary source that was located, and Firefox's lives under
        // `%APPDATA%` while this root is `%LOCALAPPDATA%`, so they stay `None`.
        (Platform::Windows, Browser::Chrome) => (Some("Google/Chrome"), WIN_NMH_DIR),
        (Platform::Windows, Browser::ChromeBeta) => (Some("Google/Chrome Beta"), WIN_NMH_DIR),
        (Platform::Windows, Browser::ChromeCanary) => (Some("Google/Chrome SxS"), WIN_NMH_DIR),
        (Platform::Windows, Browser::Chromium) => (Some("Chromium"), WIN_NMH_DIR),
        (Platform::Windows, _) => (None, WIN_NMH_DIR),
    };

    // Which of the two layouts decides where the file goes is a fact about the
    // *platform*, not about whether a presence probe exists. On Windows the
    // manifest lives in our own directory and the registry points at it, so the
    // probe above must not be allowed to steer the path — reading it that way
    // once put every Windows manifest under
    // `<root>\Google\Chrome\chat-stasher\…`, where no registry value named it
    // and no browser would ever have read it.
    let profile_root = profile_rel.map(|rel| join_rel(root, rel));
    let dir = if platform == Platform::Windows {
        join_rel(root, nmh_rel).join(browser.id())
    } else {
        match &profile_root {
            Some(probe) => join_rel(probe, nmh_rel),
            // Unreachable: every macOS and Linux arm above names a data
            // directory, and the only pair without one has returned already.
            // Answered with `None` rather than a panic, because a panic here
            // would be a crash on a path a future browser could reach.
            None => return None,
        }
    };
    Some(Target {
        browser,
        profile_root,
        manifest: dir.join(format!("{host_name}.json")),
        dir,
    })
}

/// Our own Windows manifest directory, under `%LOCALAPPDATA%`. Shared by every
/// browser there because the registry is what points at the file.
const WIN_NMH_DIR: &str = "chat-stasher/NativeMessagingHosts";

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

/// The `HKCU` vendor subkey for a browser, or `None` when this build has not got
/// one.
///
/// # Why two browsers answer `None` on Windows
///
/// The Windows half of the registration is a registry value pointing at the
/// manifest, and a browser whose key we do not know is a browser that will never
/// read the file we just wrote. A guessed key therefore produces no error at
/// all — it produces a registration that reports success and connects to
/// nothing, which is precisely the failure this command exists to avoid.
///
/// * **Chrome Beta, Chrome Canary** — S4 is positive evidence that Chromium's
///   own lookup uses one *branding-level* key (`SOFTWARE\Google\Chrome\…`,
///   `SOFTWARE\Chromium\…` under `CHROMIUM_BRANDING`), with no per-channel key
///   in that file. No primary source for a `Chrome Beta` / `Chrome SxS`
///   NativeMessagingHosts key was located, so neither is invented.
///
/// `install-native-host` prints the honest line for these — *"no registry key
/// known in this build — manifest written but NOT discoverable"* — and
/// [`Browser::support`] reports them `unverified` on Windows rather than
/// `supported`.
///
/// # Arc was here, and left on evidence, not on a guess
///
/// Arc answered `None` here too: W204 located no Arc-for-Windows key of any
/// kind (its report §8 gap 7) and left Arc `unverified` on Windows even though
/// D5 promises it. That absence was falsified on 2026-09-26 at the evidence
/// tier this module already accepts for Brave's key (S8): Anthropic's
/// Claude-in-Chrome registers Arc on Windows under
/// `HKCU\Software\ArcBrowser\Arc\NativeMessagingHosts` (**S10**), in a table
/// whose sibling keys match S1/S4/S5/S8/S9 one for one. The key is therefore
/// carried from a third-party implementation, marked `NO VENDOR DOC`, and Arc
/// reports `supported` on Windows as D5 wrote. Not located (as of 2026-09-26):
/// an Arc vendor document, or a live-registry confirmation — the entire Windows
/// registry path remains UNVERIFIED against real hardware, exactly as this
/// file's own header warns for every key here.
pub fn registry_subkey_of(browser: Browser) -> Option<&'static str> {
    match browser {
        // S1 / S4.
        Browser::Chrome => Some("Google\\Chrome"),
        // S1 / S4 (`CHROMIUM_BRANDING`).
        Browser::Chromium => Some("Chromium"),
        // S5.
        Browser::Edge => Some("Microsoft\\Edge"),
        // S8 — NO VENDOR DOC. Brave's policy key
        // `Software\Policies\BraveSoftware\Brave` is a different key and is not
        // this one.
        Browser::Brave => Some("BraveSoftware\\Brave-Browser"),
        // S9 — NO VENDOR DOC.
        Browser::Vivaldi => Some("Vivaldi"),
        // S7 — NO VENDOR DOC.
        Browser::Opera => Some("Opera Software"),
        Browser::Firefox => Some("Mozilla"),
        // S10 — NO VENDOR DOC. Arc for Windows is Chromium-based; the key is
        // carried from a third-party implementation the way Brave's is (S8):
        // no Arc vendor document for it was located, and nothing here has been
        // executed against a live registry. The refusal this arm replaced — "no
        // registry key known in this build" — was falsified, not relaxed.
        Browser::Arc => Some("ArcBrowser\\Arc"),
        Browser::ChromeBeta | Browser::ChromeCanary => None,
    }
}

/// `HKCU` subkey for a browser, or `None` when this build has not got one.
pub fn registry_key(browser: Browser, host_name: &str) -> Option<String> {
    registry_subkey_of(browser)
        .map(|vendor| format!("HKCU\\Software\\{vendor}\\NativeMessagingHosts\\{host_name}"))
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
    /// D4 (topology doc 36): the payload's install identity conflicts with the
    /// provenance already sealed in this machine's stage. Item-scope: the fix
    /// (regenerating the identity) lives in the *browser profile*, not in the
    /// host, so waiting for the host to change state can never deliver these
    /// bytes — the extension has to see the refusal and stop retrying.
    InstallConflict,
    /// EXT-13 / ADR-045: two live writers were observed sharing this
    /// `install_id` — one `report_seq` reached the host twice under different
    /// nonces, which two copies advancing independent counters cannot avoid for
    /// long and one writer cannot produce at all. Item-scope
    /// and **retryable**, unlike [`NackKind::InstallConflict`]: the bytes are
    /// not wrong, they are unattributable, and ADR-045 §3 says they stay queued
    /// until a person gives one of the copies its own identity. Marking them
    /// rejected would tell the user their capture was refused when nothing has
    /// decided that.
    IdentityConflict,
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
            NackKind::InstallConflict => "install-conflict",
            NackKind::IdentityConflict => "identity-conflict",
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
            NackKind::Integrity
            | NackKind::StageUnavailable
            | NackKind::Io
            | NackKind::IdentityConflict => true,
            NackKind::ProtocolVersion
            | NackKind::BadRequest
            | NackKind::TooLarge
            | NackKind::InvalidBundle
            | NackKind::InstallConflict
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
    /// W50c · optional content fingerprint (§6.2). Absent is the ordinary case for
    /// an older extension, and it is not an error: the shard is then recognised
    /// only by its exact bytes, which is exactly the behaviour before this field
    /// existed.
    fingerprint: Option<String>,
    /// W218 · transient account id used only by the host to derive the archive key.
    account_id: Option<String>,
    /// EXT-13 · the sender's monotonic report sequence (ADR-045). Optional, and
    /// absent is the ordinary case for an extension older than the field: the
    /// capture is still archived, it simply carries no evidence about which of
    /// two copies sent it. It is *not* persisted on the shard — the sequence is
    /// a property of the writer, not of the conversation.
    report_seq: Option<u64>,
    /// EXT-13 · the token minted with that sequence. A sequence is evidence only
    /// beside it (see [`record_report_seq`]); absent means the sender could not
    /// mint one, which is recorded as *unknown* rather than compared.
    report_nonce: Option<String>,
}

/// `has` request body (§6.6). `protocol` and `type` are checked before this is
/// parsed, so — like [`DeliverRequest`] — only the payload fields are named here.
#[derive(Debug, Deserialize)]
struct HasRequest {
    request_id: String,
    platform: String,
    session_id: String,
    fingerprint: String,
}

#[derive(Debug, Deserialize)]
struct CoordinationRequest {
    request_id: String,
    mode: String,
    platform: String,
    install_id: String,
    segment: Option<String>,
    status: Option<u16>,
    retry_after_ms: Option<u64>,
    /// W218 · raw id crosses native messaging only; the host stores its HMAC.
    account_id: Option<String>,
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + message.len());
    inner.extend(block.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(message);
    let digest = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend(block.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&digest);
    Sha256::digest(&outer).into()
}

fn account_key_from_masterkey_bytes(
    masterkey: &[u8],
    platform: &str,
    account_id: &str,
) -> Option<String> {
    if masterkey.is_empty()
        || account_id.is_empty()
        || account_id.len() > 4096
        || account_id.chars().any(char::is_control)
    {
        return None;
    }
    let salt = hmac_sha256(masterkey, b"chat-stasher/cross-install-account/salt/v1");
    let message = format!("chat-stasher/cross-install-account/v1\0{platform}\0{account_id}");
    let digest = hmac_sha256(&salt, message.as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Derive the cross-install key from the configured archive masterkey. Missing,
/// ambiguous, or unreadable key configuration deliberately means no comparable key.
fn cross_install_account_key(platform: &str, account_id: &str) -> Option<String> {
    if account_id.is_empty() || account_id.len() > 4096 || account_id.chars().any(char::is_control)
    {
        return None;
    }
    let config = Config::load().ok()?;
    let key_file = if let Some(native) = config
        .native_host
        .as_ref()
        .and_then(|h| h.destination.as_deref())
    {
        config
            .destinations
            .get(native)
            .and_then(|d| d.key_file.as_deref())
            .or(config.rustic_key_file.as_deref())?
    } else if config.destinations.is_empty() {
        config.rustic_key_file.as_deref()?
    } else if config.destinations.len() == 1 {
        config
            .destinations
            .values()
            .next()?
            .key_file
            .as_deref()
            .or(config.rustic_key_file.as_deref())?
    } else {
        return None;
    };
    let masterkey = crate::store::load_key_file(&crate::store::StoreConfig {
        key_file: PathBuf::from(key_file),
        ..Default::default()
    })
    .ok()?;
    let material = crate::store::serialize_key(&masterkey).ok()?;
    account_key_from_masterkey_bytes(material.as_bytes(), platform, account_id)
}

/// EXT-3 coordination state. Native messaging starts a fresh host process per
/// request, so SQLite's IMMEDIATE transaction is the cross-process mutex.
fn coordination(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let parsed: CoordinationRequest = match serde_json::from_value(request) {
        Ok(parsed) => parsed,
        Err(e) => return nack(request_id, NackKind::BadRequest, e.to_string()),
    };
    if !valid_request_id(&parsed.request_id)
        || parsed.install_id.is_empty()
        || parsed.install_id.len() > 128
        || parsed.platform.is_empty()
        || parsed.platform.len() > 64
        || !matches!(
            parsed.mode.as_str(),
            "claim" | "token" | "release" | "rate_limit"
        )
        || (parsed.mode == "token"
            && !matches!(parsed.segment.as_deref(), Some("enumerate" | "detail")))
        || (parsed.mode == "rate_limit" && !matches!(parsed.status, Some(403 | 429)))
        || parsed
            .account_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 4096 || id.chars().any(char::is_control))
    {
        return nack(
            request_id,
            NackKind::BadRequest,
            "malformed coordination request",
        );
    }
    let request_id = parsed.request_id.clone();
    let (machine, _) = match resolve_target() {
        HostTarget::Ready { machine, stage } => (machine, stage),
        HostTarget::Refused { kind, detail } => return nack(Some(request_id), kind, detail),
    };
    // 🔴 EXT-13 · The identity gate is **not** asked here. It is asked inside
    //    the transaction below, on the connection that holds it; the note on
    //    `coordinate_in_transaction` says why that ordering is the whole
    //    guarantee.
    let conn = match open_state_db() {
        Ok(conn) => conn,
        Err(e) => {
            return nack(
                Some(request_id),
                NackKind::Io,
                format!("cannot open coordination state: {e:#}"),
            )
        }
    };
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
        return nack(
            Some(request_id),
            NackKind::Io,
            format!("cannot lock coordination state: {e}"),
        );
    }
    let now = chrono::Utc::now().timestamp_millis();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let account_key = parsed
        .account_id
        .as_deref()
        .and_then(|id| cross_install_account_key(&parsed.platform, id))
        // reason: missing account identity or masterkey keeps arbitration platform-wide.
        .unwrap_or_default(); // reason: missing account identity or masterkey keeps arbitration platform-wide.
    let result = coordinate_in_transaction(&conn, &machine, &parsed, now, &today, &account_key);
    match result {
        Ok(CoordinationOutcome::Answered(value)) => match conn.execute_batch("COMMIT") {
            Ok(()) => value,
            Err(e) => nack(
                Some(request_id),
                NackKind::Io,
                format!("cannot save coordination state: {e}"),
            ),
        },
        Ok(CoordinationOutcome::Refused(kind, detail)) => {
            // The refusal is the answer, and the transaction is rolled back with
            // it: a claim that is not granted leaves nothing behind, not even
            // the bookkeeping the body had already done on the way there.
            let rollback = conn.execute_batch("ROLLBACK");
            if let Err(rollback_error) = rollback {
                return nack(
                    Some(request_id),
                    NackKind::Io,
                    format!("identity refusal could not be rolled back: {rollback_error}"),
                );
            }
            nack(Some(request_id), kind, detail)
        }
        Err(e) => {
            let rollback = conn.execute_batch("ROLLBACK");
            if let Err(rollback_error) = rollback {
                return nack(
                    Some(request_id),
                    NackKind::Io,
                    format!("coordination state failed: {e:#}; rollback failed: {rollback_error}"),
                );
            }
            nack(
                Some(request_id),
                NackKind::Io,
                format!("coordination state failed: {e:#}"),
            )
        }
    }
}

/// What one coordination body decided, for the caller that owns its transaction.
enum CoordinationOutcome {
    /// The request was answered; the caller commits and returns this value.
    Answered(serde_json::Value),
    /// The identity gate refused it: the caller rolls back and answers this nack.
    Refused(NackKind, String),
}

/// The coordination body, run inside the caller's `BEGIN IMMEDIATE`.
///
/// It is a function rather than a closure so a test can drive it against a
/// transaction it controls — including one another connection has already taken
/// the write lock on, which is the state a second host process is in whenever
/// two requests arrive together.
fn coordinate_in_transaction(
    conn: &rusqlite::Connection,
    machine: &str,
    parsed: &CoordinationRequest,
    now: i64,
    today: &str,
    account_key: &str,
) -> anyhow::Result<CoordinationOutcome> {
    // 🔴 EXT-13 · An install id that two live writers have been observed sharing
    //    claims no lease. Backfill is the one path that issues bulk requests
    //    against a platform, so a set of writes that cannot be attributed to one
    //    profile is exactly what must not run — and ADR-045 §3 puts the repair,
    //    not this gate, in charge of lifting it.
    //
    //    The gate is asked **here**, on the connection that holds the write lock
    //    and therefore inside the same transaction as the grant below. That is
    //    the guarantee: native messaging starts a host process per request, so
    //    another writer's conflict can be committed at any moment, and a gate
    //    consulted before the lock would answer from a state that may already
    //    have changed by the time this claim is written. `release` and
    //    `rate_limit` are deliberately not gated: they are bookkeeping about
    //    work already granted, and refusing them would leave a stale lease or an
    //    unreported cooldown behind for the next claimant.
    if matches!(parsed.mode.as_str(), "claim" | "token") {
        if let Some((kind, detail)) = identity_refusal(conn, machine, &parsed.install_id)? {
            return Ok(CoordinationOutcome::Refused(kind, detail));
        }
    }
    conn.execute(
        "DELETE FROM ext_install_v2 WHERE seen_at<=?1",
        [now - 30 * 24 * 60 * 60 * 1000],
    )?;
    conn.execute("INSERT INTO ext_install_v2(platform,install_id,account_key,seen_at) VALUES(?1,?2,?3,?4) ON CONFLICT(platform,install_id,account_key) DO UPDATE SET seen_at=excluded.seen_at", rusqlite::params![parsed.platform, parsed.install_id, account_key, now])?;
    conn.execute(
        "INSERT OR IGNORE INTO ext_platform_v2(machine,platform,account_key) VALUES(?1,?2,?3)",
        rusqlite::params![machine, parsed.platform, account_key],
    )?;
    let (owner, lease_until, cooldown_until, next_enum, next_detail, detail_day, detail_count): (Option<String>,i64,i64,i64,i64,String,i64) = conn.query_row("SELECT owner,lease_until,cooldown_until,next_enum,next_detail,detail_day,detail_count FROM ext_platform_v2 WHERE machine=?1 AND platform=?2 AND account_key=?3", rusqlite::params![machine,parsed.platform,account_key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
    conn.execute("UPDATE ext_platform_v2 SET owner=NULL,lease_until=0 WHERE machine=?1 AND platform=?2 AND account_key=?3 AND lease_until<=?4", rusqlite::params![machine,parsed.platform,account_key,now])?;
    let active: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ext_install_v2 WHERE platform=?1 AND account_key=?2 AND seen_at>?3",
        rusqlite::params![parsed.platform, account_key, now - 24 * 60 * 60 * 1000],
        |r| r.get(0),
    )?;
    let mut granted = false;
    let mut wait = 0i64;
    let mut new_cooldown = cooldown_until;
    match parsed.mode.as_str() {
        "claim" => {
            let owner_available =
                lease_until <= now || owner.as_deref() == Some(parsed.install_id.as_str());
            wait = if cooldown_until > now {
                cooldown_until - now
            } else if !owner_available {
                (lease_until - now).max(0)
            } else {
                0
            };
            granted = owner_available && wait == 0;
            if granted && wait == 0 {
                conn.execute("UPDATE ext_platform_v2 SET owner=?4,lease_until=?5 WHERE machine=?1 AND platform=?2 AND account_key=?3", rusqlite::params![machine,parsed.platform,account_key,parsed.install_id,now+120_000])?;
            }
        }
        "release" => {
            conn.execute("UPDATE ext_platform_v2 SET owner=NULL,lease_until=0 WHERE machine=?1 AND platform=?2 AND account_key=?3 AND owner=?4", rusqlite::params![machine,parsed.platform,account_key,parsed.install_id])?;
            granted = true;
        }
        "rate_limit" => {
            let header = parsed
                .retry_after_ms
                .unwrap_or(0) // reason: missing Retry-After still applies the 60s machine cooldown floor.
                .min(30 * 24 * 60 * 60 * 1000) as i64;
            new_cooldown = now + header.max(60_000);
            conn.execute("UPDATE ext_platform_v2 SET cooldown_until=MAX(cooldown_until,?4) WHERE machine=?1 AND platform=?2 AND account_key=?3", rusqlite::params![machine,parsed.platform,account_key,new_cooldown])?;
            wait = header.max(60_000);
        }
        _ => {
            let is_detail = parsed.segment.as_deref() == Some("detail");
            let owner_ok =
                owner.as_deref() == Some(parsed.install_id.as_str()) && lease_until > now;
            let cooldown_wait = (cooldown_until - now).max(0);
            let next = if is_detail { next_detail } else { next_enum };
            let daily_count = if detail_day == today { detail_count } else { 0 };
            let daily_wait = if is_detail && daily_count >= 400 {
                60_000
            } else {
                0
            };
            wait = cooldown_wait.max((next - now).max(0)).max(daily_wait);
            granted = owner_ok && wait == 0;
            if granted {
                let interval = if is_detail {
                    if active > 1 {
                        45_000
                    } else {
                        20_000
                    }
                } else if active > 1 {
                    4_000
                } else {
                    2_000
                };
                let next_at = now + interval;
                if is_detail {
                    conn.execute("UPDATE ext_platform_v2 SET lease_until=?4,next_detail=?5,detail_day=?6,detail_count=CASE WHEN detail_day=?6 THEN detail_count+1 ELSE 1 END WHERE machine=?1 AND platform=?2 AND account_key=?3", rusqlite::params![machine,parsed.platform,account_key,now+120_000,next_at,today])?;
                } else {
                    conn.execute("UPDATE ext_platform_v2 SET lease_until=?4,next_enum=?5 WHERE machine=?1 AND platform=?2 AND account_key=?3", rusqlite::params![machine,parsed.platform,account_key,now+120_000,next_at])?;
                }
            }
        }
    }
    Ok(CoordinationOutcome::Answered(
        serde_json::json!({"protocol":PROTOCOL,"type":"coordination","ok":true,"request_id":parsed.request_id,"granted":granted,"active_installs":active,"gentle":active>1,"cooldown_until":new_cooldown,"wait_ms":wait}),
    ))
}

/// EXT-13 / ADR-045 · the host's own state database, shared by every handler
/// that has to agree with another host process about one install id.
///
/// Native messaging starts a **fresh host process per request**, so a plain
/// file cannot be the mutex: two requests that arrive together would each read
/// the same base and each write their own answer, and the loser's observation
/// would simply be gone. SQLite's `BEGIN IMMEDIATE` is the cross-process mutex
/// this codebase already uses for coordination, and identity state needs
/// exactly the same guarantee — a lost update there would *hide* a detected
/// clone, which is the one direction the whole mechanism must not fail in.
const STATE_SCHEMA: &str = "PRAGMA busy_timeout=5000;
     CREATE TABLE IF NOT EXISTS ext_install(platform TEXT NOT NULL, install_id TEXT NOT NULL, seen_at INTEGER NOT NULL, PRIMARY KEY(platform,install_id));
     CREATE TABLE IF NOT EXISTS ext_platform(machine TEXT NOT NULL, platform TEXT NOT NULL, owner TEXT, lease_until INTEGER NOT NULL DEFAULT 0, cooldown_until INTEGER NOT NULL DEFAULT 0, next_enum INTEGER NOT NULL DEFAULT 0, next_detail INTEGER NOT NULL DEFAULT 0, detail_day TEXT NOT NULL DEFAULT '', detail_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(machine,platform));
     CREATE TABLE IF NOT EXISTS ext_install_v2(platform TEXT NOT NULL, install_id TEXT NOT NULL, account_key TEXT NOT NULL, seen_at INTEGER NOT NULL, PRIMARY KEY(platform,install_id,account_key));
     CREATE TABLE IF NOT EXISTS ext_platform_v2(machine TEXT NOT NULL, platform TEXT NOT NULL, account_key TEXT NOT NULL, owner TEXT, lease_until INTEGER NOT NULL DEFAULT 0, cooldown_until INTEGER NOT NULL DEFAULT 0, next_enum INTEGER NOT NULL DEFAULT 0, next_detail INTEGER NOT NULL DEFAULT 0, detail_day TEXT NOT NULL DEFAULT '', detail_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(machine,platform,account_key));
     INSERT OR IGNORE INTO ext_install_v2(platform,install_id,account_key,seen_at) SELECT platform,install_id,'',seen_at FROM ext_install;
     INSERT OR IGNORE INTO ext_platform_v2(machine,platform,account_key,owner,lease_until,cooldown_until,next_enum,next_detail,detail_day,detail_count) SELECT machine,platform,'',owner,lease_until,cooldown_until,next_enum,next_detail,detail_day,detail_count FROM ext_platform;
     CREATE TABLE IF NOT EXISTS ext_identity(machine TEXT NOT NULL, install_id TEXT NOT NULL, last_report_seq INTEGER, identity_conflict INTEGER NOT NULL DEFAULT 0, conflict_at INTEGER, conflict_evidence TEXT, seen_at INTEGER NOT NULL, PRIMARY KEY(machine, install_id));
     CREATE TABLE IF NOT EXISTS ext_identity_seq(machine TEXT NOT NULL, install_id TEXT NOT NULL, seq INTEGER NOT NULL, nonce TEXT, seen_at INTEGER NOT NULL, PRIMARY KEY(machine, install_id, seq));";

/// The state database, with its schema applied. Never created implicitly by a
/// read: a missing directory is an error the caller answers with, not a
/// silently empty state that would read as "no conflict".
fn open_state_db() -> anyhow::Result<rusqlite::Connection> {
    open_state_db_at(&crate::collect::default_state_dir())
}

/// [`open_state_db`] against a named directory, so a test can open two
/// connections to **one** database and watch what one transaction can see of the
/// other's uncommitted work.
fn open_state_db_at(state_dir: &Path) -> anyhow::Result<rusqlite::Connection> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("prepare coordination state in {}", state_dir.display()))?;
    let conn = rusqlite::Connection::open(state_dir.join("extension-coordination.sqlite3"))
        .context("open coordination state")?;
    conn.execute_batch(STATE_SCHEMA)
        .context("initialize coordination state")?;
    Ok(conn)
}

/// How many `(seq, nonce)` observations one `(machine, install_id)` retains.
///
/// The window is what makes "one sequence under two different nonces" mean
/// something: a sequence the host still remembers can be compared, and one it
/// has forgotten cannot. It is bounded because the host is not an archive of
/// every number a browser ever sent — 64 is far more than the reordering,
/// retries and worker restarts that produce a legitimate repeat can span, and
/// anything older is accepted without judgement rather than remembered forever.
///
/// Public because the protocol tests assert on both sides of the bound: that a
/// sequence **at** it is judged and one below it is not. A test that restated
/// the number would pass while the two drifted apart.
pub const IDENTITY_SEQ_WINDOW: i64 = 64;

/// What one `(report_seq, report_nonce)` observation did to the record for
/// `(machine, install_id)`.
///
/// The outcomes are different facts and stay distinct: a first value
/// establishes the key, a higher one advances it, an identical *pair* is the
/// same allocation seen twice, a sequence older than everything retained is
/// accepted without a judgement to make, and one sequence under a **different**
/// nonce is the only positive evidence the protocol has that two live writers
/// share the id. A report with no sequence is none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeqObservation {
    /// The report carried no `report_seq` — an extension older than the field.
    Unknown,
    /// The same `(seq, nonce)` this record already holds: one allocation,
    /// arriving twice. A retried or replayed frame is this, and it is not a copy.
    Retry(u64),
    /// A sequence that was not recorded and could be judged. `first` says it is
    /// the earliest one retained.
    Recorded { seq: u64, first: bool },
    /// A sequence below everything retained: there is no recorded nonce to
    /// compare it against, so it is accepted and **not** judged.
    Unjudged(u64),
    /// One sequence, two different nonces.
    Conflict(u64),
}

/// What the host knows about one `(machine, install_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdentityRecord {
    /// `None` means no report has ever carried a sequence. It is *unknown*, and
    /// deliberately not `0`: zero is a value a writer can send, and a fabricated
    /// one would be indistinguishable from a writer that has never reported.
    last_report_seq: Option<u64>,
    identity_conflict: bool,
}

/// Fold one `(report_seq, report_nonce)` observation into the record for
/// `(machine, install_id)`, inside the caller's transaction.
///
/// **A sequence is evidence only together with the nonce minted for it.** Two
/// copies start from the same stored value and advance independently, so they
/// send the *same numbers*, and a number that repeats also arises from one
/// writer whose frame is retried or replayed. Comparing sequences therefore
/// says nothing on its own — and treating a regression as the signal meant a
/// merely late arrival, a timeout, or a restarted worker accused an honest
/// install of being a copy. What is compared here is the **pair**: a repeat of
/// a pair already recorded is one allocation arriving twice, while one sequence
/// under a different nonce is two allocations of the same number, which no
/// single writer can produce.
///
/// The conflict flag is **sticky**. Two writers that have proved they share an
/// id do not become one writer again because the next report happens to order
/// correctly — that is exactly what the *other* copy's report looks like. The
/// only way out is a new install id, which is a new key here and starts clean.
fn record_report_seq(
    conn: &rusqlite::Connection,
    machine: &str,
    install_id: &str,
    seq: Option<u64>,
    nonce: Option<&str>,
    now_ms: i64,
) -> anyhow::Result<(IdentityRecord, SeqObservation)> {
    let existing = conn
        .query_row(
            "SELECT last_report_seq, identity_conflict, conflict_at FROM ext_identity WHERE machine=?1 AND install_id=?2",
            rusqlite::params![machine, install_id],
            |row| {
                Ok((
                    // reason: an unrepresentable stored sequence is no recorded sequence.
                    row.get::<_, Option<i64>>(0)?.and_then(|v| u64::try_from(v).ok()),
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    let (previous, was_conflicted, first_conflict_at) = existing.unwrap_or((None, false, None));
    let observation = match seq {
        None => SeqObservation::Unknown,
        Some(value) => {
            let stored = conn
                .query_row(
                    "SELECT nonce FROM ext_identity_seq WHERE machine=?1 AND install_id=?2 AND seq=?3",
                    rusqlite::params![machine, install_id, value as i64],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            match stored {
                // 🔴 Only two *known* and different nonces are evidence. A side
                //    that is absent is not a second value — it is the host not
                //    having been told — so it is never the difference that
                //    accuses a writer of being a copy.
                Some(recorded) => match (recorded.as_deref(), nonce) {
                    (Some(recorded), Some(arriving)) if recorded != arriving => {
                        SeqObservation::Conflict(value)
                    }
                    // The sequence is known but its nonce was never sent. The
                    // first arrival that names one is not a second allocation,
                    // so it is learned rather than judged.
                    (None, Some(arriving)) => {
                        conn.execute(
                            "UPDATE ext_identity_seq SET nonce=?4, seen_at=?5 WHERE machine=?1 AND install_id=?2 AND seq=?3",
                            rusqlite::params![
                                machine,
                                install_id,
                                value as i64,
                                arriving,
                                now_ms
                            ],
                        )?;
                        SeqObservation::Retry(value)
                    }
                    _ => SeqObservation::Retry(value),
                },
                None => {
                    let floor: Option<i64> = conn
                        .query_row(
                            "SELECT MIN(seq) FROM ext_identity_seq WHERE machine=?1 AND install_id=?2",
                            rusqlite::params![machine, install_id],
                            |row| row.get(0),
                        )
                        .optional()?
                        .flatten();
                    if floor.is_some_and(|floor| (value as i64) < floor) {
                        // Below everything retained ⇒ there is no recorded nonce
                        // to compare against and no judgement to make. It is not
                        // recorded either: the window would prune it on the way
                        // in, so storing it would only claim a memory this id
                        // does not keep.
                        SeqObservation::Unjudged(value)
                    } else {
                        conn.execute(
                            "INSERT OR REPLACE INTO ext_identity_seq(machine,install_id,seq,nonce,seen_at) VALUES(?1,?2,?3,?4,?5)",
                            rusqlite::params![machine, install_id, value as i64, nonce, now_ms],
                        )?;
                        SeqObservation::Recorded {
                            seq: value,
                            first: previous.is_none(),
                        }
                    }
                }
            }
        }
    };
    let (next, conflicted, evidence) = match observation {
        SeqObservation::Unknown | SeqObservation::Retry(_) | SeqObservation::Unjudged(_) => {
            (previous, was_conflicted, None)
        }
        SeqObservation::Recorded { seq, .. } => (
            Some(previous.map_or(seq, |previous| previous.max(seq))),
            was_conflicted,
            None,
        ),
        SeqObservation::Conflict(value) => (
            previous,
            true,
            Some(format!(
                "report_seq {value} arrived with a different report_nonce than the one recorded for it: two live writers allocated the same sequence independently"
            )),
        ),
    };
    let conflict_at = if conflicted {
        first_conflict_at.or(Some(now_ms))
    } else {
        None
    };
    conn.execute(
        "INSERT INTO ext_identity(machine,install_id,last_report_seq,identity_conflict,conflict_at,conflict_evidence,seen_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(machine,install_id) DO UPDATE SET
           last_report_seq=COALESCE(excluded.last_report_seq, ext_identity.last_report_seq),
           identity_conflict=excluded.identity_conflict,
           conflict_at=excluded.conflict_at,
           conflict_evidence=COALESCE(excluded.conflict_evidence, ext_identity.conflict_evidence),
           seen_at=excluded.seen_at",
        rusqlite::params![
            machine,
            install_id,
            next.map(|value| value as i64),
            conflicted,
            conflict_at,
            evidence,
            now_ms
        ],
    )?;
    // The window, and only the window, decides what a repeat can be judged
    // against — so it is applied on every fold rather than when it happens to
    // be convenient.
    conn.execute(
        "DELETE FROM ext_identity_seq WHERE machine=?1 AND install_id=?2 AND seq NOT IN (SELECT seq FROM ext_identity_seq WHERE machine=?1 AND install_id=?2 ORDER BY seq DESC LIMIT ?3)",
        rusqlite::params![machine, install_id, IDENTITY_SEQ_WINDOW],
    )?;
    // A conflicted row is **never** pruned. Forgetting one would re-admit an id
    // already known to be shared, and the next collision would have to be
    // re-observed from two reports instead of one.
    conn.execute(
        "DELETE FROM ext_identity WHERE identity_conflict=0 AND seen_at<=?1",
        [now_ms - 30 * 24 * 60 * 60 * 1000],
    )?;
    // An identity the table above has just forgotten cannot be compared against
    // anything, so its window is not evidence and does not outlive it.
    conn.execute(
        "DELETE FROM ext_identity_seq WHERE NOT EXISTS (SELECT 1 FROM ext_identity i WHERE i.machine=ext_identity_seq.machine AND i.install_id=ext_identity_seq.install_id)",
        [],
    )?;
    Ok((
        IdentityRecord {
            last_report_seq: next,
            identity_conflict: conflicted,
        },
        observation,
    ))
}

/// Read-only `(machine, install_id)` identity state, outside any transaction.
fn read_identity_state(
    conn: &rusqlite::Connection,
    machine: &str,
    install_id: &str,
) -> anyhow::Result<IdentityRecord> {
    let found: Option<IdentityRecord> = conn
        .query_row(
            "SELECT last_report_seq, identity_conflict FROM ext_identity WHERE machine=?1 AND install_id=?2",
            rusqlite::params![machine, install_id],
            |row| {
                Ok(IdentityRecord {
                    // reason: an unrepresentable stored sequence is no recorded sequence.
                    last_report_seq: row
                        .get::<_, Option<i64>>(0)?
                        .and_then(|v| u64::try_from(v).ok()),
                    identity_conflict: row.get(1)?,
                })
            },
        )
        .optional()?;
    Ok(found.unwrap_or(IdentityRecord {
        last_report_seq: None,
        identity_conflict: false,
    }))
}

/// What a refusal tells the person reading it, in one place: the two callers
/// that can answer it — a lease claim and a delivery — have to say the same
/// thing, and a second copy of this sentence is a second thing to keep true.
const IDENTITY_CONFLICT_DETAIL: &str = "this install_id is shared by more than one live copy: two writers sent the same report sequence, so neither copy can be told from the other. Nothing was archived and the capture stays queued. Give this profile a new identity in the extension popup; captures already archived keep the old identity";

/// The host's answer to "may this install id act?", read **on the caller's own
/// connection** and therefore inside the caller's transaction.
///
/// That is the whole point of the signature. A gate that opened its own
/// connection would answer from a snapshot taken *before* the caller's
/// transaction began, and a conflict another host process commits while this
/// request waits for the lock would be invisible to it — so a lease would be
/// granted, or a bundle sealed, on the strength of a state that had already
/// changed. Reading through the connection that holds the write lock makes the
/// question and the action it authorises one step.
///
/// ADR-045 §3 requires the copy to rekey before further status writes, lease
/// claims, or delivery, and requires the refusal to be fail-closed — the
/// capture stays queued, never dropped, and never archived under an identity
/// that cannot be attributed to one profile.
///
/// `None` means the id is not known to be shared, which is not the same as
/// "only one writer exists": until the first collision two copies are
/// indistinguishable, and the ADR says so in as many words.
fn identity_refusal(
    conn: &rusqlite::Connection,
    machine: &str,
    install_id: &str,
) -> anyhow::Result<Option<(NackKind, String)>> {
    if read_identity_state(conn, machine, install_id)?.identity_conflict {
        Ok(Some((
            NackKind::IdentityConflict,
            IDENTITY_CONFLICT_DETAIL.to_string(),
        )))
    } else {
        Ok(None)
    }
}

/// `identity_state` — does the host know this install id to be shared?
///
/// Read-only and cheap, so the popup can ask once per open **without** writing
/// anything. That matters for the state the ADR is most concerned about: an
/// install whose backfill is switched off never sends a status report at all,
/// so a flag that only ever rode on a report could not reach its popup.
fn identity_state(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let Some(request_id) = request
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return nack(request_id, NackKind::BadRequest, "missing request_id");
    };
    if !valid_request_id(&request_id) {
        return nack(
            Some(request_id),
            NackKind::BadRequest,
            "malformed request_id",
        );
    }
    let Some(install_id) = request.get("install_id").and_then(|v| v.as_str()) else {
        return nack(Some(request_id), NackKind::BadRequest, "missing install_id");
    };
    if install_id.len() != 36
        || !install_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return nack(
            Some(request_id),
            NackKind::BadRequest,
            "malformed install_id",
        );
    }
    let machine = match resolve_target() {
        HostTarget::Ready { machine, .. } => machine,
        HostTarget::Refused { kind, detail } => return nack(Some(request_id), kind, detail),
    };
    let conn = match open_state_db() {
        Ok(conn) => conn,
        Err(e) => {
            return nack(
                Some(request_id),
                NackKind::Io,
                format!("cannot read identity state: {e:#}"),
            )
        }
    };
    match read_identity_state(&conn, &machine, install_id) {
        Ok(state) => serde_json::json!({
            "protocol": PROTOCOL,
            "type": "identity_state",
            "ok": true,
            "request_id": request_id,
            "identity_conflict": state.identity_conflict,
        }),
        Err(e) => nack(
            Some(request_id),
            NackKind::Io,
            format!("cannot read identity state: {e:#}"),
        ),
    }
}

/// `[A-Za-z0-9_-]{1,128}` (§6.2).
fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// EXT-13 · `report_nonce`, the token minted with a `report_seq`.
///
/// Deliberately the same grammar as a request id: an opaque, bounded token the
/// host only ever compares for equality, never parses. What matters is that the
/// host rejects an empty one, since an empty string is exactly what a writer
/// would send if it had no random source to mint from — and "I have no nonce"
/// is `report_nonce` being **absent**, which is a different message.
fn valid_nonce(nonce: &str) -> bool {
    valid_request_id(nonce)
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

    // A config that cannot be used is a `nack config` here, exactly as it was
    // when the loader reported the failure through `source`: the host answers the
    // browser over a protocol, not a terminal, so it cannot exit non-zero at
    // anyone. The difference is that the reason now reaches the popup verbatim
    // (file, line, what is wrong with it) instead of a sentence saying only that
    // it "could not be read or parsed".
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            return refused(
                NackKind::Config,
                format!(
                    "{e:#}; no stage is configured until that file is fixed, then re-run \
                     `chat-stasher install-native-host --stage <path>`"
                ),
            );
        }
    };
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
        Some("status") => status_report(request, echoed_id),
        // §6.6 — the content question, answered from the stage (W50c).
        Some("has") => has(request, echoed_id),
        // EXT-3: host-persisted, machine-wide backfill arbitration.
        Some("coordination") => coordination(request, echoed_id),
        // §6.4/§6.5 — the two parameterless read-only queries.
        Some("summary") => match no_parameters(&request) {
            Ok(()) => summary(echoed_id),
            Err(detail) => nack(echoed_id, NackKind::BadRequest, detail),
        },
        Some("open_dashboard") => match no_parameters(&request) {
            Ok(()) => open_dashboard(echoed_id),
            Err(detail) => nack(echoed_id, NackKind::BadRequest, detail),
        },
        Some("identity_state") => identity_state(request, echoed_id),
        Some("other_installs") => other_installs(request, echoed_id),
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

const OTHER_INSTALLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const OTHER_INSTALLS_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

/// Read the configured archive through the existing overview implementation,
/// then return only the count of status records other than this install. The
/// child output and its per-platform fields never leave the host process.
fn other_installs(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let Some(request_id) = request
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| valid_request_id(id))
        .map(str::to_owned)
    else {
        return nack(request_id, NackKind::BadRequest, "malformed `request_id`");
    };
    let Some(install_id) = request
        .get("install_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
    else {
        return nack(
            Some(request_id),
            NackKind::BadRequest,
            "malformed `install_id`",
        );
    };
    let Some(object) = request.as_object() else {
        return nack(
            Some(request_id),
            NackKind::BadRequest,
            "request is not an object",
        );
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "protocol" | "type" | "request_id" | "install_id"
        )
    }) {
        return nack(
            Some(request_id),
            NackKind::BadRequest,
            "unknown other_installs request field",
        );
    }
    let config = match Config::load() {
        Ok(config) => config,
        Err(_) => {
            return nack(
                Some(request_id),
                NackKind::Config,
                "the configured archive could not be opened",
            )
        }
    };
    let destination = match dashboard_destination(&config) {
        Ok(name) => name,
        Err(_) => {
            return nack(
                Some(request_id),
                NackKind::Config,
                "no readable dashboard destination is configured",
            )
        }
    };
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            return nack(
                Some(request_id),
                NackKind::Io,
                "the archive reader could not be started",
            )
        }
    };
    let output = match run_overview_summary(&binary, &destination) {
        Ok(output) => output,
        Err(()) => {
            return nack(
                Some(request_id),
                NackKind::Io,
                "the archive status count could not be read",
            )
        }
    };
    let Some(count) = other_install_count(&output, install_id) else {
        return nack(
            Some(request_id),
            NackKind::Io,
            "the archive status count was incomplete",
        );
    };
    serde_json::json!({
        "protocol": PROTOCOL,
        "type": "other_installs",
        "ok": true,
        "request_id": request_id,
        "count": count,
    })
}

fn run_overview_summary(binary: &Path, destination: &str) -> Result<Vec<u8>, ()> {
    use std::io::Read as _;
    use std::process::Stdio;
    let mut child = std::process::Command::new(binary)
        .args([
            "overview",
            "--destination",
            destination,
            "--json",
            "--summary",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let mut stdout = child.stdout.take().ok_or(())?;
    let reader = std::thread::spawn(move || -> Result<Vec<u8>, ()> {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut oversized = false;
        loop {
            let count = stdout.read(&mut buffer).map_err(|_| ())?;
            if count == 0 {
                return if oversized { Err(()) } else { Ok(bytes) };
            }
            if bytes.len().saturating_add(count) > OTHER_INSTALLS_OUTPUT_LIMIT {
                oversized = true;
            } else if !oversized {
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
    });
    let deadline = std::time::Instant::now() + OTHER_INSTALLS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                drop(child.kill());
                drop(child.wait());
                drop(reader.join());
                return Err(());
            }
        }
    };
    let bytes = reader.join().map_err(|_| ())??;
    if !status.success() && status.code() != Some(1) {
        return Err(());
    }
    Ok(bytes)
}

fn other_install_count(output: &[u8], current_install_id: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_slice(output).ok()?;
    if value.get("command").and_then(serde_json::Value::as_str) != Some("overview")
        || value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        || value.get("variant").and_then(serde_json::Value::as_str) != Some("summary")
    {
        return None;
    }
    let installs = value.get("installs")?.as_array()?;
    let mut skipped_current = false;
    let mut count = 0u64;
    for install in installs {
        let id = install.get("install_id")?.as_str()?;
        if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return None;
        }
        if id == current_install_id && !skipped_current {
            skipped_current = true;
        } else {
            count = count.checked_add(1)?;
        }
    }
    Some(count)
}

/// Atomically replace one install's content-free latest status in the stage.
fn status_report(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let parsed: serde_json::Value = request;
    if !parsed
        .get("request_id")
        .and_then(|v| v.as_str())
        .is_some_and(valid_request_id)
    {
        return nack(request_id, NackKind::BadRequest, "malformed `request_id`");
    }
    let Some(status) = parsed.get("status").and_then(|v| v.as_object()) else {
        return nack(request_id, NackKind::BadRequest, "missing status object");
    };
    let Some(install_id) = status.get("install_id").and_then(|v| v.as_str()) else {
        return nack(request_id, NackKind::BadRequest, "missing install_id");
    };
    if install_id.len() != 36
        || !install_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-')
        || status
            .get("browser")
            .and_then(|v| v.as_str())
            .is_none_or(|v| v.is_empty() || v.len() > 80)
        || status
            .get("profile_label")
            .is_none_or(|v| !v.is_null() && !v.as_str().is_some_and(|s| s.len() <= 80))
        || status
            .get("extension_version")
            .and_then(|v| v.as_str())
            .is_none_or(|v| v.is_empty() || v.len() > 80)
        || status
            .get("reported_at")
            .and_then(|v| v.as_str())
            .is_none_or(|v| v.len() > 40 || chrono::DateTime::parse_from_rfc3339(v).is_err())
        || status
            .get("platforms")
            .and_then(|v| v.as_array())
            .is_none_or(|v| v.len() > 100)
        // EXT-13: optional, and absent is a real state — "this extension
        // predates the field", which the host accepts and records as unknown.
        // A present-but-not-a-sequence value is malformed, not unknown.
        || status
            .get("report_seq")
            .is_some_and(|v| v.as_u64().is_none())
        // EXT-13 · The nonce names the allocation a sequence was minted with, so
        // it is meaningful only beside one. A nonce without a sequence is a
        // message that believes it supplied evidence and did not — malformed,
        // not a state the host could record.
        || status
            .get("report_nonce")
            .is_some_and(|v| v.as_str().is_none_or(|s| !valid_nonce(s)))
        || (status.get("report_nonce").is_some() && status.get("report_seq").is_none())
    {
        return nack(request_id, NackKind::BadRequest, "malformed status fields");
    }
    if status.keys().any(|key| {
        !matches!(
            key.as_str(),
            "install_id"
                | "browser"
                | "profile_label"
                | "extension_version"
                | "reported_at"
                | "report_seq"
                | "report_nonce"
                | "platforms"
        )
    }) {
        return nack(request_id, NackKind::BadRequest, "unknown status field");
    }
    for row in status["platforms"].as_array().expect("validated array") {
        if row.get("platform").and_then(|v| v.as_str()).is_none()
            || row
                .get("captured_by_this_browser")
                .and_then(|v| v.as_u64())
                .is_none()
            || row.get("pending").and_then(|v| v.as_u64()).is_none()
            || row
                .get("paused_reason")
                .is_some_and(|v| !v.is_null() && !v.as_str().is_some_and(|s| s.len() <= 120))
            || row
                .get("account_fingerprint")
                .is_some_and(|v| v.as_str().is_none_or(|s| !valid_sha256(s)))
        {
            return nack(
                request_id,
                NackKind::BadRequest,
                "malformed platform status row",
            );
        }
        if row.as_object().is_none_or(|o| {
            o.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "platform"
                        | "captured_by_this_browser"
                        | "pending"
                        | "paused_reason"
                        | "account_fingerprint"
                )
            })
        }) {
            return nack(
                request_id,
                NackKind::BadRequest,
                "unknown platform status field",
            );
        }
    }
    let Some(request_id) = parsed
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return nack(request_id, NackKind::BadRequest, "missing request_id");
    };
    let (machine, stage) = match resolve_target() {
        HostTarget::Ready { machine, stage } => (machine, stage),
        HostTarget::Refused { kind, detail } => return nack(Some(request_id), kind, detail),
    };
    let dir = stage.join("ext-status");
    let result = (|| -> anyhow::Result<()> {
        // ① EXT-13 · The sequence is recorded in the host's own state database
        //    **before** the staged file is written. Two copies reporting at the
        //    same moment would otherwise both read the same base and the later
        //    write would erase the earlier one's observation — and the
        //    observation erased would be the conflict, which is the one
        //    direction this must never fail in. The staged record is what the
        //    dashboard reads; this is what the gates read, and deriving one from
        //    the other would make the gates depend on a file the stage may not
        //    be able to accept. What this fold concluded stays in the database:
        //    the report's own copy is written below, out of what the database
        //    holds *then*, never out of the snapshot read here.
        let conn = open_state_db()?;
        with_immediate(&conn, || {
            record_report_seq(
                &conn,
                &machine,
                install_id,
                status.get("report_seq").and_then(|v| v.as_u64()),
                status.get("report_nonce").and_then(|v| v.as_str()),
                chrono::Utc::now().timestamp_millis(),
            )
        })?;

        // ① Status is keyed by `(machine, install_id)`: the machine is the
        //    directory, so a record's own `machine` is not a claim the caller
        //    can make and a copy cannot overwrite another machine's report.
        let keyed_dir = dir.join(&machine);
        fs::create_dir_all(&keyed_dir)?;
        let target = keyed_dir.join(format!("{install_id}.json"));
        let (previous, migrated_from_legacy) =
            previous_status(&dir, &keyed_dir, &machine, install_id);
        let (daily_streak, reported_daily) = daily_report_history(
            &previous,
            status["reported_at"]
                .as_str()
                .expect("validated reported_at"),
        )?;
        let mut value = parsed["status"].clone();
        value["machine"] = serde_json::Value::String(machine.clone());
        value["schema"] = serde_json::Value::String("chat-stasher/ext-status@1".into());
        value["daily_report_streak"] = serde_json::Value::from(daily_streak);
        value["reported_daily"] = serde_json::Value::Bool(reported_daily);
        if migrated_from_legacy {
            value["legacy_migration"] = serde_json::Value::Bool(true);
        }
        // 🔴 `report_seq` is the caller's own observation and is written back
        //    **verbatim**, including its absence. Writing the host's
        //    high-water mark here instead would make the file say the record's
        //    sequence is something the record's writer never sent, and the
        //    high-water mark is already the host's business in its own state.
        publish_status_record(&conn, &machine, install_id, value, &keyed_dir, &target)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            serde_json::json!({"protocol":PROTOCOL,"type":"status","ok":true,"request_id":request_id})
        }
        Err(e) => nack(
            Some(request_id),
            NackKind::Io,
            format!("cannot persist extension status: {e}"),
        ),
    }
}

/// Publish one staged status record — the copy the dashboard reads — carrying
/// the conflict flag **as the state database holds it when the file is written**.
///
/// ④ The flag is read and the file renamed inside **one** transaction, and the
/// flag is not the one the report read when it recorded its own sequence. The
/// two are not the same fact, and the difference is a slow writer: a report can
/// be delayed between its own record and this write for reasons that have
/// nothing to do with a copy (a busy disk, a suspended process, a stage on a
/// network mount), and two copies reporting at once can commit their
/// observations in one order and write their copies in the other. Reading the
/// flag from the report's own snapshot is what would let that slower writer
/// erase the conflict a faster copy had already published — leaving a dashboard
/// that shows a clean install whose captures the host is refusing.
///
/// The lock is what makes the read and the write one step: a conflict committed
/// after this transaction began cannot be committed *during* it, so a copy can
/// only publish "no conflict" while no conflict exists, never after one was
/// published. And the flag is sticky in the database (see `record_report_seq`),
/// so a report that carries no conflict of its own still publishes the one the
/// database holds — which is the point, not an accident.
///
/// Called after the record commits, so a stage that cannot accept the file takes
/// the report's copy down without touching what the host learned.
fn publish_status_record(
    conn: &rusqlite::Connection,
    machine: &str,
    install_id: &str,
    mut value: serde_json::Value,
    keyed_dir: &Path,
    target: &Path,
) -> anyhow::Result<()> {
    with_immediate(conn, || {
        let identity = read_identity_state(conn, machine, install_id)?;
        value["identity_conflict"] = serde_json::Value::Bool(identity.identity_conflict);
        if identity.identity_conflict {
            value["identity_conflict_evidence"] = serde_json::Value::String(
                "two live writers share this install id: one report sequence reached it twice with different nonces".into(),
            );
        }
        let bytes = serde_json::to_vec(&value)?;
        // 🔴 Written to a temporary file in the destination directory and renamed
        //    over the record, so a reader never sees half a record. Ordering two
        //    writers' renames is the lock's doing, above.
        let mut temp = tempfile::NamedTempFile::new_in(keyed_dir)?;
        use std::io::Write as _;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(target).map_err(|e| e.error)?;
        Ok(())
    })
}

/// The previous observation of one install, and whether it came from the legacy
/// flat layout.
///
/// EXT-13 · Before `(machine, install_id)` keying there was exactly one file per
/// install, `ext-status/<install_id>.json`, written only by the machine whose
/// stage this is. It is read here as a **legacy observation of this machine**,
/// which is why the record has to name the machine that wrote it: one whose
/// `machine` disagrees is not this machine's observation of this install, and
/// adopting its history would attribute another machine's reporting cadence to
/// this one. Its own file is left exactly where it is — the stage is not the
/// archive, and a migration that deleted it would destroy the only local copy
/// of a report whose successor may never be written.
fn previous_status(
    dir: &Path,
    keyed_dir: &Path,
    machine: &str,
    install_id: &str,
) -> (serde_json::Value, bool) {
    if let Ok(bytes) = fs::read(keyed_dir.join(format!("{install_id}.json"))) {
        // reason: an unparseable keyed record means no readable history for the streak.
        return (
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            false,
        );
    }
    match fs::read(dir.join(format!("{install_id}.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    {
        Some(legacy) if legacy.get("machine").and_then(|v| v.as_str()) == Some(machine) => {
            (legacy, true)
        }
        // reason: a legacy record written by another machine, or with no
        // readable machine, has no readable history to inherit here.
        _ => (serde_json::Value::Null, false),
    }
}

/// Run `body` inside `BEGIN IMMEDIATE`, committing on success and rolling back
/// on failure. The lock is the cross-process mutex (`coordination`'s argument,
/// which applies to every table in this file).
fn with_immediate<T>(
    conn: &rusqlite::Connection,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("lock identity state")?;
    match body() {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .context("save identity state")?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rollback) = conn.execute_batch("ROLLBACK") {
                return Err(e.context(format!(
                    "identity state failed; rollback failed: {rollback}"
                )));
            }
            Err(e)
        }
    }
}

/// Track whether an install has established daily reporting without treating
/// multiple same-day status refreshes as separate days. Once established, the
/// historical fact remains true even after the install goes silent.
fn daily_report_history(
    previous: &serde_json::Value,
    current_reported_at: &str,
) -> anyhow::Result<(u64, bool)> {
    let current_at = chrono::DateTime::parse_from_rfc3339(current_reported_at)?.timestamp();
    let previous_streak = previous
        .get("daily_report_streak")
        .and_then(serde_json::Value::as_u64)
        // reason: Missing history contains no observed report samples.
        .unwrap_or(0);
    let was_daily = previous
        .get("reported_daily")
        .and_then(serde_json::Value::as_bool)
        // reason: Missing history has not established daily reporting cadence.
        .unwrap_or(false);
    let previous_at = previous
        .get("reported_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp());
    Ok(match previous_at {
        Some(previous_at) => {
            let elapsed = current_at.saturating_sub(previous_at);
            if (18 * 60 * 60..=30 * 60 * 60).contains(&elapsed) {
                (
                    previous_streak.saturating_add(1),
                    was_daily || previous_streak >= 2,
                )
            } else if elapsed > 30 * 60 * 60 {
                (1, was_daily)
            } else {
                (previous_streak.max(1), was_daily)
            }
        }
        None => (1, was_daily),
    })
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
    if let Some(fingerprint) = parsed.fingerprint.as_deref() {
        if !valid_sha256(fingerprint) {
            return nack(request_id, NackKind::BadRequest, "malformed `fingerprint`");
        }
    }
    if parsed
        .account_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > 4096 || id.chars().any(char::is_control))
    {
        return nack(request_id, NackKind::BadRequest, "malformed `account_id`");
    }
    // EXT-13 · The nonce names the allocation its sequence was minted with, so
    // it is meaningful only beside one — the same rule §6.7 applies to a status
    // report, for the same reason.
    if parsed
        .report_nonce
        .as_deref()
        .is_some_and(|nonce| !valid_nonce(nonce))
    {
        return nack(request_id, NackKind::BadRequest, "malformed `report_nonce`");
    }
    if parsed.report_nonce.is_some() && parsed.report_seq.is_none() {
        return nack(
            request_id,
            NackKind::BadRequest,
            "`report_nonce` without `report_seq`",
        );
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
    let bundle: serde_json::Value = match serde_json::from_str(&parsed.payload) {
        Ok(value) => value,
        Err(e) => {
            return nack(
                request_id,
                NackKind::InvalidBundle,
                format!("invalid JSON bundle: {e}"),
            )
        }
    };
    // reason: validated legacy bundles without a platform cannot produce a cross-platform key.
    let platform = bundle
        .get("platform")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default(); // reason: validated legacy bundles without a platform cannot produce a cross-platform key.
    let account_key = parsed
        .account_id
        .as_deref()
        .and_then(|id| cross_install_account_key(platform, id));

    // ③ EXT-13 · **The sequence, the conflict gate and the seal are one
    //    transaction.** A delivery is wire traffic too, and for an install whose
    //    backfill is switched off it is the *only* wire traffic there is: that
    //    install never runs a tick, so it never sends a status report, and this
    //    is the only place its sequence reaches the host at all.
    //
    //    Recording the sequence and then sealing outside the lock would leave a
    //    window in which another host process commits a conflict — and the
    //    bundle would be archived on the strength of a state that had already
    //    changed. Holding `BEGIN IMMEDIATE` across the seal closes it: a writer
    //    that wants to record for this id waits for this seal to finish.
    //
    //    Lock order is state database, then stage (`seal_payload` takes the
    //    stage lock), and nothing in this file takes them the other way round.
    //    The cost is that a second process can wait here for the length of one
    //    seal; SQLite's `busy_timeout` is the bound, and the alternative — an
    //    archive that lands after the identity it carries was already known to
    //    be ambiguous — is the failure this exists to prevent.
    //
    //    Every outcome commits the identity record, including a failed seal: the
    //    observation is real whatever the stage did with the bytes, and a
    //    conflict that was just detected must not be rolled back. Only a
    //    database-level failure rolls back, and then nothing is sealed either.
    let conn = match open_state_db() {
        Ok(conn) => conn,
        Err(e) => {
            return nack(
                request_id,
                NackKind::Io,
                format!("cannot open coordination state: {e:#}"),
            )
        }
    };
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
        return nack(
            request_id,
            NackKind::Io,
            format!("cannot lock coordination state: {e}"),
        );
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let outcome = (|| -> anyhow::Result<DeliveryOutcome> {
        if let Some(install_id) = inbox::bundle_install_id(parsed.payload.as_bytes()) {
            let (state, _) = record_report_seq(
                &conn,
                &machine,
                &install_id,
                parsed.report_seq,
                parsed.report_nonce.as_deref(),
                now_ms,
            )?;
            if state.identity_conflict {
                return Ok(DeliveryOutcome::Refused);
            }
        }
        Ok(
            match inbox::seal_payload(
                &parsed.name,
                parsed.payload.as_bytes(),
                &stage,
                &machine,
                crate::store::DEFAULT_SHARD_BUCKET_CAP,
                parsed.fingerprint.as_deref(),
                account_key.as_deref(),
            ) {
                Ok(sealed) => DeliveryOutcome::Sealed(Box::new(sealed)),
                Err(error) => DeliveryOutcome::SealFailed(error),
            },
        )
    })();
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Err(rollback) = conn.execute_batch("ROLLBACK") {
                return nack(
                    request_id,
                    NackKind::Io,
                    format!("cannot record delivery identity: {e:#}; rollback failed: {rollback}"),
                );
            }
            return nack(
                request_id,
                NackKind::Io,
                format!("cannot record delivery identity: {e:#}"),
            );
        }
    };
    if let Err(e) = conn.execute_batch("COMMIT") {
        if let Err(rollback) = conn.execute_batch("ROLLBACK") {
            return nack(
                request_id,
                NackKind::Io,
                format!("cannot save delivery identity: {e}; rollback failed: {rollback}"),
            );
        }
        return nack(
            request_id,
            NackKind::Io,
            format!("cannot save delivery identity: {e}"),
        );
    }

    match outcome {
        // The refusal is *retryable* on purpose: the bytes are not wrong, they
        // are unattributable, and ADR-045 §3 keeps them queued until a person
        // gives one of the copies its own identity. Rejecting them would say a
        // decision had been made about the capture when none has.
        DeliveryOutcome::Refused => nack(
            request_id,
            NackKind::IdentityConflict,
            IDENTITY_CONFLICT_DETAIL.to_string(),
        ),
        DeliveryOutcome::Sealed(sealed) => match *sealed {
            inbox::SealOutcome::Stored(consumed) => serde_json::json!({
                "protocol": PROTOCOL,
                "type": "ack",
                "request_id": parsed.request_id,
                "status": "stored",
                "sha256": parsed.sha256,
                "shard": consumed.shard,
            }),
            inbox::SealOutcome::Duplicate(existing) => serde_json::json!({
                "protocol": PROTOCOL,
                "type": "ack",
                "request_id": parsed.request_id,
                "status": "duplicate",
                "sha256": parsed.sha256,
                "shard": existing.matched_shard,
            }),
        },
        DeliveryOutcome::SealFailed(inbox::SealError::Lock(e)) => {
            nack(request_id, NackKind::StageUnavailable, format!("{e:#}"))
        }
        DeliveryOutcome::SealFailed(inbox::SealError::IdentityCollision) => nack(
            request_id,
            NackKind::InstallConflict,
            "this install_id is already registered to a different browser/profile label; regenerate the install identity in the later browser profile",
        ),
        DeliveryOutcome::SealFailed(inbox::SealError::Other(e)) => {
            nack(request_id, NackKind::Io, format!("{e:#}"))
        }
    }
}

/// What one `deliver` decided about the bundle, once its identity is recorded.
enum DeliveryOutcome {
    /// The install id is known to be shared: nothing was sealed.
    Refused,
    /// Boxed to keep this enum small: the sealed outcome carries a whole shard
    /// record, and it would otherwise be three orders of magnitude larger than
    /// the two refusals beside it.
    Sealed(Box<inbox::SealOutcome>),
    SealFailed(inbox::SealError),
}

/// `has` — §6.6, W50c · **does the stage already hold this exact content?**
///
/// The question the extension used to answer from its own `storage.local`
/// memory ("we remember acking this fingerprint"), and could not answer safely:
/// the memory is keyed by content and destination, so an archive that was
/// replaced or restored at the same path left every record standing and a
/// conversation could be settled as archived without one byte reaching the new
/// archive. Here the answer comes from the only thing that cannot be stale
/// relative to the archive — the archive.
///
/// Three outcomes, and they stay three:
///  · `ok` with `held: true` — a shard in that conversation's directory carries
///    this fingerprint; `shard` names it, so the answer can be audited;
///  · `ok` with `held: false` — **asked and answered**: nothing there holds it.
///    An empty or recreated stage lands here, and the extension delivers;
///  · `nack` — the question could not be asked (no stage, unreadable directory,
///    malformed request). The extension reads every one of those as "not known to
///    be held" ⇒ deliver, so a host that cannot answer never causes a skip.
///
/// 🔴 A request that names a different conversation than its fingerprint came from
///    is not an error this can detect, and does not need to be: the answer is
///    scoped to the directory named by `platform`/`session_id`, so a mismatch can
///    only ever produce `held: false` — one extra copy, never a wrong skip.
fn has(request: serde_json::Value, request_id: Option<String>) -> serde_json::Value {
    let parsed: HasRequest = match serde_json::from_value(request) {
        Ok(parsed) => parsed,
        Err(e) => return nack(request_id, NackKind::BadRequest, e.to_string()),
    };
    if !valid_request_id(&parsed.request_id) {
        return nack(request_id, NackKind::BadRequest, "malformed `request_id`");
    }
    if !valid_sha256(&parsed.fingerprint) {
        return nack(request_id, NackKind::BadRequest, "malformed `fingerprint`");
    }
    // Bounded rather than charset-restricted: the values are path components only
    // after `session_dir_id` has sanitised them, and restricting them further here
    // would reject a request for a bundle `deliver` would happily accept.
    for (field, value) in [
        ("platform", parsed.platform.as_str()),
        ("session_id", parsed.session_id.as_str()),
    ] {
        if value.is_empty() || value.len() > 512 {
            return nack(
                request_id,
                NackKind::BadRequest,
                format!("`{field}` must be 1-512 characters"),
            );
        }
    }
    let request_id = Some(parsed.request_id.clone());

    let (machine, stage) = match resolve_target() {
        HostTarget::Ready { machine, stage } => (machine, stage),
        HostTarget::Refused { kind, detail } => return nack(request_id, kind, detail),
    };

    let dir = crate::store::session_shard_dir(
        &stage,
        &machine,
        &inbox::session_dir_id(&parsed.platform, &parsed.session_id),
    );
    match inbox::hold_lookup(&dir, &parsed.fingerprint) {
        Ok(found) => serde_json::json!({
            "protocol": PROTOCOL,
            "type": "has",
            "ok": true,
            "request_id": parsed.request_id,
            "held": found.is_some(),
            "shard": found,
        }),
        // A directory that exists but cannot be read is *not* "nothing is held".
        // `io` is host-scope and retryable: the item stays pending, and the
        // extension delivers it rather than skipping it (§6.3).
        Err(e) => nack(request_id, NackKind::Io, format!("{e:#}")),
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
        crate::json_out::TimeState::NoConversationContent => {
            serde_json::json!({"kind": "no_conversation_content"})
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
///
/// Public because `status`'s local layer reports the same stage sessions the
/// `summary` protocol answer does; a second walk of `<stage>/sessions/` would be
/// a second definition of "what counts as a stage session".
pub fn scan_stage(stage: &Path) -> StageScan {
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
            // A defensive branch kept for an impossible status: `ui` no longer
            // exits 1 (W172/OQ-2 — an archive that holds nothing serves its
            // honest empty page and exits 0). Older builds exited 1 here, and
            // the sentence says which older behaviour was observed rather
            // than narrating a live path that cannot happen.
            "it exited 1 — a status this build's `ui` does not produce (an older build \
             exited 1 when the destination was read in full and held nothing to show; \
             it now serves that destination's empty page instead)"
                .to_string()
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
    // The same gate `hello` uses: a host that cannot name its stage is not
    // configured, and a dashboard launched from it would be a second,
    // differently-configured view of the same archive.
    if let HostTarget::Refused { kind, detail } = resolve_target() {
        return nack(request_id, kind, detail);
    }
    // Reached only once `resolve_target()` did **not** refuse, which already
    // covers every config that cannot be read — so an `Err` here means the file
    // changed between the two reads of this one request, and refusing is the only
    // honest answer to that as well.
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => return nack(request_id, NackKind::Config, format!("{e:#}")),
    };
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
    fn hmac_sha256_matches_rfc_4231_sample() {
        let digest = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn cross_install_account_key_is_masterkey_scoped_and_never_exists_without_a_key() {
        let first = account_key_from_masterkey_bytes(
            b"synthetic-masterkey",
            "claude",
            "synthetic-account-a",
        )
        .unwrap();
        let second_install = account_key_from_masterkey_bytes(
            b"synthetic-masterkey",
            "claude",
            "synthetic-account-a",
        )
        .unwrap();
        let other_account = account_key_from_masterkey_bytes(
            b"synthetic-masterkey",
            "claude",
            "synthetic-account-b",
        )
        .unwrap();
        let other_platform = account_key_from_masterkey_bytes(
            b"synthetic-masterkey",
            "chatgpt",
            "synthetic-account-a",
        )
        .unwrap();
        assert_eq!(first, second_install);
        assert_ne!(first, other_account);
        assert_ne!(first, other_platform);
        assert!(account_key_from_masterkey_bytes(b"", "claude", "synthetic-account-a").is_none());
    }

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
    fn daily_report_history_needs_three_daily_samples_and_remembers_established_cadence() {
        let first = serde_json::json!({"reported_at":"2026-09-24T12:00:00Z"});
        let (streak, reported_daily) =
            daily_report_history(&first, "2026-09-25T13:00:00Z").unwrap();
        assert_eq!((streak, reported_daily), (1, false));

        let second =
            serde_json::json!({"reported_at":"2026-09-25T13:00:00Z", "daily_report_streak":1});
        let (streak, reported_daily) =
            daily_report_history(&second, "2026-09-26T12:00:00Z").unwrap();
        assert_eq!((streak, reported_daily), (2, false));

        let third =
            serde_json::json!({"reported_at":"2026-09-26T12:00:00Z", "daily_report_streak":2});
        let (streak, reported_daily) =
            daily_report_history(&third, "2026-09-27T12:00:00Z").unwrap();
        assert_eq!((streak, reported_daily), (3, true));

        let same_day = serde_json::json!({"reported_at":"2026-09-27T12:00:00Z", "daily_report_streak":3, "reported_daily":true});
        assert_eq!(
            daily_report_history(&same_day, "2026-09-27T12:05:00Z").unwrap(),
            (3, true)
        );
        let stale = serde_json::json!({"reported_at":"2026-09-27T12:05:00Z", "daily_report_streak":3, "reported_daily":true});
        assert_eq!(
            daily_report_history(&stale, "2026-09-30T12:05:00Z").unwrap(),
            (1, true)
        );
    }

    #[test]
    fn other_install_count_excludes_one_matching_install_and_preserves_unknown() {
        let current = "11111111-1111-4111-8111-111111111111";
        let output = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "command": "overview",
            "variant": "summary",
            "installs": [
                {"install_id": current},
                {"install_id": "22222222-2222-4222-8222-222222222222"},
                {"install_id": "33333333-3333-4333-8333-333333333333"}
            ]
        }))
        .unwrap();
        assert_eq!(other_install_count(&output, current), Some(2));

        let no_other = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "command": "overview",
            "variant": "summary",
            "installs": [{"install_id": current}]
        }))
        .unwrap();
        assert_eq!(other_install_count(&no_other, current), Some(0));

        let incomplete = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "command": "overview",
            "variant": "summary",
            "installs": [{"install_id": "not-a-valid-install-id"}]
        }))
        .unwrap();
        assert_eq!(other_install_count(&incomplete, current), None);
    }

    #[test]
    fn pinned_chrome_id_is_a_wellformed_chromium_id() {
        assert!(validate_chromium_extension_id(CHROME_EXTENSION_ID).is_ok());
        assert!(validate_chromium_extension_id("tooshort").is_err());
        // `z` is outside a-p.
        assert!(validate_chromium_extension_id(&"z".repeat(32)).is_err());
    }

    // ------------------------------------------------- discovery-root shape

    /// The root a home directory implies is a fact about the home, not about
    /// the machine reading it.
    ///
    /// This is the property `nativehost_doctor_test.rs` was silently relying on
    /// while it was red on `windows-latest`: it hands the doctor a `tempfile`
    /// home and expects the answer to be about *that* home. Asserted for all
    /// three layouts here, from whichever platform is running, because the shape
    /// is a pure function of `(platform, home)` and a Windows-only assertion
    /// would leave two of the three unchecked.
    #[test]
    fn the_root_is_derived_from_the_home_it_is_given() {
        let unix_home = Path::new("/home/someone");
        assert_eq!(
            default_root(Platform::Macos, unix_home),
            unix_home.join("Library").join("Application Support")
        );
        assert_eq!(default_root(Platform::Linux, unix_home), unix_home);

        // A Windows home is spelled with backslashes; `Path::join` appends with
        // whatever the *host* separator is, so compare component-wise rather
        // than against a literal path string.
        let windows_home = Path::new(r"C:\Users\someone");
        let windows_root = default_root(Platform::Windows, windows_home);
        assert_eq!(
            windows_root,
            windows_home.join("AppData").join("Local"),
            "the Windows root must be under the home it was given, not wherever \
             this process happens to point"
        );
    }

    /// `%LOCALAPPDATA%` is the machine's answer and wins on Windows — but only
    /// on Windows, and only when it is actually set.
    ///
    /// The environment read is injected rather than performed, so the rule can
    /// be asserted without mutating a process-global that other tests in this
    /// binary share.
    #[test]
    fn a_redirected_known_folder_wins_on_windows_and_nowhere_else() {
        let home = Path::new("/home/someone");

        // Set: the known folder is authoritative, because a profile can
        // redirect it away from `<home>\AppData\Local`.
        let redirected = OsStr::new(r"D:\LocalAppData");
        assert_eq!(
            machine_root_with(Platform::Windows, home, Some(redirected)),
            PathBuf::from(redirected)
        );

        // Unset or empty: fall back to the home-derived root.
        assert_eq!(
            machine_root_with(Platform::Windows, home, None),
            home.join("AppData").join("Local")
        );
        assert_eq!(
            machine_root_with(Platform::Windows, home, Some(OsStr::new(""))),
            home.join("AppData").join("Local"),
            "an empty variable is not a root"
        );

        // The variable is a Windows known folder; elsewhere it is not a fact
        // about the layout at all and must not move the root.
        assert_eq!(
            machine_root_with(Platform::Macos, home, Some(redirected)),
            home.join("Library").join("Application Support")
        );
        assert_eq!(
            machine_root_with(Platform::Linux, home, Some(redirected)),
            home
        );
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

    // ------------------------------------------------------------ EXT-13

    /// The conflict gate answers **inside the caller's transaction**.
    ///
    /// Native messaging starts a host process per request, so another writer's
    /// conflict can be committed at any moment. A gate that opened its own
    /// connection would answer from a snapshot taken before the caller's
    /// transaction began; a conflict committed while this request waited for the
    /// lock would then be invisible to it, and the claim would be granted on the
    /// strength of a state that had already changed.
    ///
    /// The holder below is a second connection that already owns the write lock
    /// — exactly the position another host process is in — and its conflict is
    /// committed while the claim is waiting on that lock. Both connections are
    /// opened *before* the lock is taken, because opening one applies the state
    /// schema, which writes: an open on the far side of the lock would itself
    /// wait, and the claim would then never be between its check and its commit.
    #[test]
    fn a_claim_waiting_for_the_lock_sees_a_conflict_committed_while_it_waits() {
        use std::sync::mpsc;
        use std::time::Duration;

        const MACHINE: &str = "w248-machine";
        const INSTALL: &str = "11111111-1111-4111-8111-111111111111";

        let dir = tempfile::tempdir().expect("temp state dir");
        let holder = open_state_db_at(dir.path()).expect("open the holding connection");
        let claimant = open_state_db_at(dir.path()).expect("open the claiming connection");

        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the second writer takes the write lock");
        record_report_seq(&holder, MACHINE, INSTALL, Some(5), Some("nonce-a"), 1)
            .expect("the original allocates sequence 5");
        record_report_seq(&holder, MACHINE, INSTALL, Some(5), Some("nonce-b"), 2)
            .expect("the copy allocates sequence 5 as well");
        // Uncommitted, and therefore visible to no other connection at all: the
        // conflict exists only inside this transaction.

        let claim = CoordinationRequest {
            request_id: "w248-claim".into(),
            mode: "claim".into(),
            platform: "chatgpt".into(),
            install_id: INSTALL.into(),
            segment: None,
            status: None,
            retry_after_ms: None,
            account_id: None,
        };
        let (attempting, waiting) = mpsc::channel();
        let outcome: anyhow::Result<CoordinationOutcome> = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let conn = claimant;
                attempting.send(()).expect("announce the attempt");
                // 🔴 This blocks until the holder commits: `BEGIN IMMEDIATE`
                //    cannot start while another connection holds the write lock.
                conn.execute_batch("BEGIN IMMEDIATE")
                    .expect("the claim takes the lock");
                let outcome =
                    coordinate_in_transaction(&conn, MACHINE, &claim, 3, "2026-09-29", "");
                if matches!(outcome, Ok(CoordinationOutcome::Refused(..))) {
                    conn.execute_batch("ROLLBACK")
                        .expect("a refused claim leaves nothing behind");
                }
                outcome
            });
            waiting.recv().expect("the claim announced itself");
            // A scheduling margin, not a premise. The worker has already built
            // the request it is about to send and needs microseconds to reach
            // the lock; this thread only has to commit. Losing the race would
            // not make the assertion wrong — the conflict is committed either
            // way, and the fixed gate refuses either way — it would only make it
            // say less.
            std::thread::sleep(Duration::from_millis(250));
            holder.execute_batch("COMMIT").expect("commit the conflict");
            worker.join().expect("the claim thread joins")
        });
        assert!(
            matches!(
                outcome.expect("the claim body runs"),
                CoordinationOutcome::Refused(NackKind::IdentityConflict, _)
            ),
            "a claim may not be granted on a conflict that was already committed when the lock came free"
        );
    }

    /// ④ The staged record carries the conflict flag the database holds **when
    /// the file is written**, not the flag the report read when it recorded its
    /// own sequence. Those are two different facts, and the gap between them is
    /// a slow writer: the review's interleaving is a report whose own
    /// observation is clean, delayed past the collision another copy commits.
    /// Publishing from the report's own snapshot is what lets that report erase
    /// the conflict — and the direction it fails in is the bad one, because the
    /// host is refusing this install's captures while the dashboard it writes
    /// says the install is fine.
    ///
    /// The publisher here waits for the state lock, so nothing in this test
    /// depends on a race: only a writer that reads the flag *outside* the lock
    /// can see the state as it was before `copying` began.
    #[test]
    fn a_stale_report_cannot_publish_away_a_committed_conflict() {
        use std::sync::mpsc;
        use std::time::Duration;

        const MACHINE: &str = "w248-machine";
        const INSTALL: &str = "11111111-1111-4111-8111-111111111111";

        let state = tempfile::tempdir().expect("temp state dir");
        let stage = tempfile::tempdir().expect("temp stage dir");
        let keyed = stage.path().join(MACHINE);
        fs::create_dir_all(&keyed).expect("the machine's record directory");
        let target = keyed.join(format!("{INSTALL}.json"));

        let reporting = open_state_db_at(state.path()).expect("open the reporting connection");
        let copying = open_state_db_at(state.path()).expect("open the copying connection");

        // One writer's own report: a sequence nobody else has used, recorded and
        // committed with no conflict anywhere.
        with_immediate(&reporting, || {
            record_report_seq(&reporting, MACHINE, INSTALL, Some(11), Some("stale"), 1)
        })
        .expect("the report records its own sequence");

        // The copy allocates the same number under a nonce of its own. Still
        // uncommitted, so the connection above cannot see it — which is exactly
        // the position a slower report is in while this commits.
        copying
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the copy takes the write lock");
        let (_, observation) =
            record_report_seq(&copying, MACHINE, INSTALL, Some(11), Some("copy"), 2)
                .expect("the copy allocates sequence 11 as well");
        assert_eq!(
            observation,
            SeqObservation::Conflict(11),
            "the fixture must really collide, or the assertion below proves nothing"
        );

        let value = serde_json::json!({
            "install_id": INSTALL,
            "schema": "chat-stasher/ext-status@1",
        });
        let (published_at, target_for_publish) = (keyed.clone(), target.clone());
        let (attempting, waiting) = mpsc::channel();
        let published: anyhow::Result<()> = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                attempting.send(()).expect("announce the publish");
                publish_status_record(
                    &reporting,
                    MACHINE,
                    INSTALL,
                    value,
                    &published_at,
                    &target_for_publish,
                )
            });
            waiting.recv().expect("the publisher announced itself");
            // A scheduling margin, not a premise. The publisher needs
            // microseconds to reach the lock and this thread only has to commit;
            // losing the race would not make the assertion wrong — the conflict
            // is committed either way, and a publisher that reads it under the
            // lock publishes it either way — it would only make the test say
            // less.
            std::thread::sleep(Duration::from_millis(250));
            copying
                .execute_batch("COMMIT")
                .expect("commit the collision");
            worker.join().expect("the publisher thread joins")
        });
        published.expect("the staged record is written");

        assert!(
            read_identity_state(&copying, MACHINE, INSTALL)
                .expect("read the committed state")
                .identity_conflict,
            "the fixture must leave a committed conflict behind"
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(&target).expect("the staged record exists"))
                .expect("the staged record is JSON");
        assert_eq!(
            record["install_id"], INSTALL,
            "the published record is the report's own copy: {record}"
        );
        assert_eq!(
            record["identity_conflict"], true,
            "a report delayed past the collision must not publish a copy that denies it: {record}"
        );
        assert!(
            record.get("identity_conflict_evidence").is_some(),
            "the record says which observation set the flag: {record}"
        );
    }
}
