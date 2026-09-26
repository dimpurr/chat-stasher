//! The browser × OS registration matrix (decision **D5** of
//! `.private/docs/36-EXTENSION-TOPOLOGY.md` §5).
//!
//! This file exists because the alternative — a path table asserted only by the
//! code that uses it — cannot fail when a path is *wrong*. A wrong discovery
//! path produces no error at all: the manifest lands in a directory the browser
//! never reads, the extension reports "native messaging host not found", and
//! every test in `b98_nmhost_test.rs` still passes because the command did
//! exactly what it was told. So the expected paths are written out **here**, as
//! the documented strings, and compared against what [`nativehost::target`]
//! computes. Changing a path then means changing this table too, deliberately.
//!
//! Every row is resolved against a `tempfile` home through
//! [`nativehost::default_root`] — the pure `(platform, home)` function — so no
//! real browser directory is read on any platform, and the same assertions run
//! on macOS, Linux and Windows CI. The one place a resolved path is *not*
//! asserted is the pair with no path at all, which is asserted as `None`.
//!
//! What this file does **not** claim: that any of these paths has been observed
//! on a live install of the browser in question. The macOS paths are verified
//! against this machine's own `~/Library/Application Support` — on 2026-09-26
//! **all ten** browsers' discovery directories exist there, Arc's at the
//! `Arc/User Data/…` shape, which is the one entry with third-party evidence
//! only; the Linux and Windows shapes are verified
//! against the vendor documentation cited in `nativehost.rs`'s header, plus the
//! third-party evidence recorded there for the four browsers that have no vendor
//! document. See the W204 report §3 for the per-path state.

use chat_stasher::nativehost::{
    self, registry_key, registry_subkey_of, Browser, Platform, Support, HOST_NAME,
};
use std::path::{Path, PathBuf};

/// One row of the matrix: the browser, the OS, the path components *below* that
/// OS's discovery root, and the D5 tier.
///
/// The components are joined one at a time exactly as `join_rel` does, so the
/// expectation carries the host OS's separator rather than a `/` that only
/// matches Unix.
struct Row {
    browser: Browser,
    platform: Platform,
    /// Empty means this build has no path for the pair.
    components: &'static [&'static str],
    tier: Support,
}

const fn row(
    browser: Browser,
    platform: Platform,
    components: &'static [&'static str],
    tier: Support,
) -> Row {
    Row {
        browser,
        platform,
        components,
        tier,
    }
}

/// The matrix, written as the documented strings. Each Chromium-family entry is
/// `<data dir>/NativeMessagingHosts`; Firefox's Linux directory is spelled
/// lowercase-with-hyphens while its macOS one is CamelCase, which is the kind of
/// difference that is invisible until a user reports the host is never found.
const MATRIX: &[Row] = &[
    // ---- macOS:  <root> = ~/Library/Application Support -------------------
    row(
        Browser::Chrome,
        Platform::Macos,
        &["Google/Chrome", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Chromium,
        Platform::Macos,
        &["Chromium", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Edge,
        Platform::Macos,
        &["Microsoft Edge", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Brave,
        Platform::Macos,
        &["BraveSoftware/Brave-Browser", "NativeMessagingHosts"],
        Support::Supported,
    ),
    // Arc is the one Chromium fork whose application-data root carries an extra
    // `User Data` component. Dropping it produces a manifest nothing reads.
    row(
        Browser::Arc,
        Platform::Macos,
        &["Arc/User Data", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::ChromeBeta,
        Platform::Macos,
        &["Google/Chrome Beta", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::ChromeCanary,
        Platform::Macos,
        &["Google/Chrome Canary", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Opera,
        Platform::Macos,
        &["com.operasoftware.Opera", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Vivaldi,
        Platform::Macos,
        &["Vivaldi", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Firefox,
        Platform::Macos,
        &["Mozilla", "NativeMessagingHosts"],
        Support::Supported,
    ),
    // ---- Linux:  <root> = $HOME ------------------------------------------
    row(
        Browser::Chrome,
        Platform::Linux,
        &[".config/google-chrome", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Chromium,
        Platform::Linux,
        &[".config/chromium", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Edge,
        Platform::Linux,
        &[".config/microsoft-edge", "NativeMessagingHosts"],
        Support::Supported,
    ),
    row(
        Browser::Brave,
        Platform::Linux,
        &[
            ".config/BraveSoftware/Brave-Browser",
            "NativeMessagingHosts",
        ],
        Support::Supported,
    ),
    row(
        Browser::ChromeBeta,
        Platform::Linux,
        &[".config/google-chrome-beta", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::ChromeCanary,
        Platform::Linux,
        &[".config/google-chrome-canary", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Opera,
        Platform::Linux,
        &[".config/opera", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Vivaldi,
        Platform::Linux,
        &[".config/vivaldi", "NativeMessagingHosts"],
        Support::Unverified,
    ),
    row(
        Browser::Firefox,
        Platform::Linux,
        &[".mozilla", "native-messaging-hosts"],
        Support::Supported,
    ),
    // Arc ships for macOS and Windows only. `None`, and asserted as such rather
    // than omitted: an omitted row is how "we do not look there" becomes
    // "there is nothing there".
    Row {
        browser: Browser::Arc,
        platform: Platform::Linux,
        components: &[],
        tier: Support::Unverified,
    },
    // ---- Windows:  <root> = %LOCALAPPDATA%, manifest in our own directory --
    // The registry points at the file, so every browser's manifest lives under
    // our own tree, one subdirectory per browser, whatever its data directory
    // is. A browser with no registry key is `Unverified` even when D5 promises
    // it: a manifest no key names is a file no browser reads.
    row(
        Browser::Chrome,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/chrome"],
        Support::Supported,
    ),
    row(
        Browser::Chromium,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/chromium"],
        Support::Supported,
    ),
    row(
        Browser::Edge,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/edge"],
        Support::Supported,
    ),
    row(
        Browser::Brave,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/brave"],
        Support::Supported,
    ),
    row(
        Browser::Arc,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/arc"],
        Support::Unverified,
    ),
    row(
        Browser::ChromeBeta,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/chrome-beta"],
        Support::Unverified,
    ),
    row(
        Browser::ChromeCanary,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/chrome-canary"],
        Support::Unverified,
    ),
    row(
        Browser::Opera,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/opera"],
        Support::Unverified,
    ),
    row(
        Browser::Vivaldi,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/vivaldi"],
        Support::Unverified,
    ),
    row(
        Browser::Firefox,
        Platform::Windows,
        &["chat-stasher/NativeMessagingHosts/firefox"],
        Support::Supported,
    ),
];

const PLATFORMS: [Platform; 3] = [Platform::Macos, Platform::Linux, Platform::Windows];

/// Join `components` onto `root` one component at a time, so the expectation
/// uses the host OS's separator, and append the manifest file name.
fn expected_manifest(root: &Path, components: &[&str]) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in components {
        for part in component.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
        }
    }
    path.join(format!("{HOST_NAME}.json"))
}

/// Every browser × platform pair is in the table, and the table has no pair the
/// enum does not.
///
/// Without this, adding a variant to `Browser` would leave it silently absent
/// from a table whose whole purpose is to be complete — and the tests below,
/// which iterate the table, would keep passing.
#[test]
fn the_matrix_covers_every_browser_and_platform_exactly_once() {
    assert_eq!(
        MATRIX.len(),
        Browser::ALL.len() * PLATFORMS.len(),
        "the table must be exactly Browser::ALL × platforms"
    );
    for browser in Browser::ALL {
        for platform in PLATFORMS {
            let rows: Vec<&Row> = MATRIX
                .iter()
                .filter(|row| row.browser == browser && row.platform == platform)
                .collect();
            assert_eq!(
                rows.len(),
                1,
                "{} × {} has {} rows",
                browser.id(),
                platform.id(),
                rows.len()
            );
        }
    }
}

/// The path this build resolves is the path the row documents.
///
/// A `tempfile` home, through [`nativehost::default_root`] — the pure
/// `(platform, home)` answer — so this reads nothing on the machine it runs on
/// and gives the same verdict on all three CI platforms.
#[test]
fn every_resolved_path_is_the_documented_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    for row in MATRIX {
        let root = nativehost::default_root(row.platform, &home);
        let resolved = nativehost::target(row.platform, &root, row.browser, HOST_NAME);
        if row.components.is_empty() {
            assert!(
                resolved.is_none(),
                "{} × {}: the table says there is no path here, and this build \
                 resolved {}",
                row.browser.id(),
                row.platform.id(),
                resolved.unwrap().manifest.display()
            );
            continue;
        }
        let target = resolved.unwrap_or_else(|| {
            panic!(
                "{} × {}: the table documents a path and this build has none",
                row.browser.id(),
                row.platform.id()
            )
        });
        assert_eq!(
            target.manifest,
            expected_manifest(&root, row.components),
            "{} × {}",
            row.browser.id(),
            row.platform.id()
        );
        // The manifest is always inside the directory the browser is told to
        // look in, and never a directory of its own.
        assert_eq!(target.manifest.parent(), Some(target.dir.as_path()));
    }
}

/// The tier the code reports is the tier the table says.
///
/// This is the assertion that stops D5 from becoming decoration: a browser
/// silently promoted to `supported` would ship as a promise, and one silently
/// demoted would ship as a browser users are told not to trust.
#[test]
fn the_support_tier_is_the_one_the_matrix_documents() {
    for row in MATRIX {
        let actual = row.browser.support(row.platform);
        if row.components.is_empty() {
            assert_eq!(
                actual,
                None,
                "{} × {}: no path means no tier, not a tier",
                row.browser.id(),
                row.platform.id()
            );
            continue;
        }
        assert_eq!(
            actual,
            Some(row.tier),
            "{} × {}",
            row.browser.id(),
            row.platform.id()
        );
    }
}

/// Having a path and having a tier are the same question, asked twice.
///
/// `Browser::has_path` is a hand-written mirror of the `target()` table, which
/// is exactly the kind of duplicate that drifts. This is the pin.
#[test]
fn the_path_table_and_has_path_agree_on_every_pair() {
    let tmp = tempfile::tempdir().unwrap();
    for browser in Browser::ALL {
        for platform in PLATFORMS {
            let root = nativehost::default_root(platform, tmp.path());
            let resolvable = nativehost::target(platform, &root, browser, HOST_NAME).is_some();
            assert_eq!(
                resolvable,
                browser.has_path(platform),
                "{} × {}",
                browser.id(),
                platform.id()
            );
            assert_eq!(
                resolvable,
                browser.support(platform).is_some(),
                "{} × {}: a pair with a tier must have a path",
                browser.id(),
                platform.id()
            );
        }
    }
}

/// A browser with no registry key is never reported `supported` on Windows.
///
/// The Windows manifest is only reachable through the registry, so a
/// `supported` tier for a keyless browser would be a promise that cannot be
/// kept — and, worse, one that `install-native-host` reports as *written*.
/// Chrome Beta, Chrome Canary and Arc are the three: see
/// [`nativehost::registry_subkey_of`] for why each returns `None` instead of a
/// guess.
#[test]
fn windows_never_promises_a_browser_it_has_no_registry_key_for() {
    for browser in Browser::ALL {
        // One direction only, and it is the direction that matters: `supported`
        // on Windows implies a key exists. The converse is deliberately *not*
        // asserted — Opera and Vivaldi have keys and are still `unverified`,
        // because a third-party-sourced key is not evidence enough to promise
        // anything.
        if browser.support(Platform::Windows) == Some(Support::Supported) {
            assert!(
                registry_subkey_of(browser).is_some(),
                "{} is reported supported on Windows and has no registry key, so the \
                 manifest this build writes is a file no browser reads",
                browser.id()
            );
        }
    }

    // The three that refuse to guess, named so that removing one from the list
    // above is a deliberate act rather than an oversight.
    for browser in [Browser::ChromeBeta, Browser::ChromeCanary, Browser::Arc] {
        assert_eq!(
            registry_subkey_of(browser),
            None,
            "{} must not get an invented Windows registry key",
            browser.id()
        );
        assert_eq!(
            browser.support(Platform::Windows),
            Some(Support::Unverified),
            "{}: with no registry key the honest tier is unverified",
            browser.id()
        );
    }
}

/// The keys that *are* known are the documented ones, per browser.
#[test]
fn the_known_windows_registry_keys_are_the_documented_ones() {
    let expected: [(Browser, &str); 7] = [
        (
            Browser::Chrome,
            "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts",
        ),
        // Chromium reads its own key first and falls back to Chrome's (S4).
        (
            Browser::Chromium,
            "HKCU\\Software\\Chromium\\NativeMessagingHosts",
        ),
        (
            Browser::Edge,
            "HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts",
        ),
        (
            Browser::Brave,
            "HKCU\\Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts",
        ),
        (
            Browser::Vivaldi,
            "HKCU\\Software\\Vivaldi\\NativeMessagingHosts",
        ),
        (
            Browser::Opera,
            "HKCU\\Software\\Opera Software\\NativeMessagingHosts",
        ),
        (
            Browser::Firefox,
            "HKCU\\Software\\Mozilla\\NativeMessagingHosts",
        ),
    ];
    for (browser, prefix) in expected {
        assert_eq!(
            registry_key(browser, HOST_NAME),
            Some(format!("{prefix}\\{HOST_NAME}")),
            "{}",
            browser.id()
        );
        // Per-user on purpose: `HKLM` would need an elevated process, and an
        // installer that needs elevation to register a per-user host is an
        // installer users will not run.
        assert!(!registry_key(browser, HOST_NAME).unwrap().contains("HKLM"));
    }
}

/// Presence is three-valued, and only the browsers whose data directory a
/// primary source names get an answer.
///
/// `None` is the answer for a platform with no cheap probe, and it must stay
/// distinct from `Some(false)`: unknown is not absence, and a caller that
/// collapsed them would skip the browsers it cannot see.
#[test]
fn presence_is_answered_only_where_a_source_names_the_data_directory() {
    let tmp = tempfile::tempdir().unwrap();

    // macOS and Linux: every browser with a path has a probe, because the data
    // directory and the manifest directory are the same tree.
    for platform in [Platform::Macos, Platform::Linux] {
        for browser in Browser::ALL {
            let Some(target) = nativehost::target(platform, tmp.path(), browser, HOST_NAME) else {
                continue;
            };
            assert_eq!(
                target.detected(),
                Some(false),
                "{} × {}: an empty root is not a browser",
                browser.id(),
                platform.id()
            );
            assert!(target.profile_root.is_some());
            assert!(!target.browser_present());
        }
    }

    // Windows: S2 names the data directory for the four Chrome-family browsers
    // and nothing else, so the rest are `None` — unknown — and
    // `browser_present` answers `true` for them, which is the safe direction:
    // unknown is not evidence of absence, so the manifest is written.
    for browser in Browser::ALL {
        let target = nativehost::target(Platform::Windows, tmp.path(), browser, HOST_NAME)
            .expect("every browser has a Windows manifest path");
        let expected = if matches!(
            browser,
            Browser::Chrome | Browser::ChromeBeta | Browser::ChromeCanary | Browser::Chromium
        ) {
            Some(false)
        } else {
            None
        };
        assert_eq!(
            target.detected(),
            expected,
            "{}: presence probe",
            browser.id()
        );
        assert_eq!(
            target.browser_present(),
            expected.unwrap_or(true),
            "{}: unknown presence must not read as absence",
            browser.id()
        );
    }
}

/// The manifest directory on Windows never follows the presence probe.
///
/// The regression guard for a bug this change introduced and
/// `windows_shape_writes_json_and_prints_the_hkcu_registry_command` caught:
/// giving four browsers a `profile_rel` on Windows made the path builder take
/// the "per-browser discovery directory" branch, so every Windows manifest
/// landed under `<root>\Google\Chrome\chat-stasher\…` — a path no registry
/// value named and no browser would ever read, while the command reported
/// `wrote`.
#[test]
fn windows_manifests_live_in_our_own_directory_whatever_the_probe_says() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("localappdata");
    std::fs::create_dir_all(root.join("Google").join("Chrome")).unwrap();

    for browser in Browser::ALL {
        let target = nativehost::target(Platform::Windows, &root, browser, HOST_NAME).unwrap();
        assert_eq!(
            target.dir,
            root.join("chat-stasher")
                .join("NativeMessagingHosts")
                .join(browser.id()),
            "{}",
            browser.id()
        );
        // The data directory is still reported — it is why the browser is
        // written for at all — but it is never where the manifest goes.
        if let Some(probe) = &target.profile_root {
            assert!(
                !target.manifest.starts_with(probe),
                "{}: the manifest must not be placed inside the browser's own data \
                 directory on Windows",
                browser.id()
            );
        }
    }
}
