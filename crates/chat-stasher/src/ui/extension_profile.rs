//! Local-only lookup and launch for a precisely matched Chromium profile.
//!
//! The extension can supply a display label, but it cannot read the browser's
//! profile directory. The dashboard therefore opens a local instance only when
//! the profile can be named without guessing: either the label uniquely matches
//! Chromium's profile metadata and no other install of that browser claims it,
//! or the label decides nothing and this machine holds exactly one install for
//! that browser while exactly one local profile has the extension installed. An
//! ambiguous candidate set resolves to no target rather than to a guess. No
//! label-to-directory guess is persisted or sent to another machine.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTarget {
    pub(crate) browser_id: String,
    pub(crate) profile_directory: String,
}

impl OpenTarget {
    fn application(&self) -> Option<&'static str> {
        match self.browser_id.as_str() {
            "arc" => Some("Arc"),
            "brave" => Some("Brave Browser"),
            "chrome" => Some("Google Chrome"),
            "chromium" => Some("Chromium"),
            "edge" => Some("Microsoft Edge"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileCandidate {
    directory: String,
    name: String,
    has_extension: bool,
}

/// One browser's profile metadata as this machine reads it: directory name →
/// profile.
type ProfileMap = BTreeMap<String, ProfileCandidate>;

/// What one archived install record says about this machine, once its browser
/// has been resolved to the id the local profile metadata is keyed by.
struct LocalInstall<'a> {
    /// `None` when the record carries no readable install id. It is still an
    /// install of this browser on this machine — which is exactly the thing a
    /// uniqueness claim may not assume away — it simply has no key to file a
    /// launch target under.
    install_id: Option<&'a str>,
    browser_id: &'static str,
    /// `None` is the absence of a claim: a record with no `profile_label`, one
    /// whose label is `null`, and one whose label is blank all leave the profile
    /// to be found another way.
    profile_label: Option<&'a str>,
}

impl<'a> LocalInstall<'a> {
    /// `None` for a record that is not this machine's, or whose browser this
    /// machine has no profile metadata key for.
    fn of(install: &'a serde_json::Value, local_machine: &str) -> Option<Self> {
        if install.get("machine").and_then(serde_json::Value::as_str) != Some(local_machine) {
            return None;
        }
        let browser_id = browser_id(install.get("browser").and_then(serde_json::Value::as_str)?)?;
        Some(Self {
            install_id: install
                .get("install_id")
                .and_then(serde_json::Value::as_str),
            browser_id,
            profile_label: install
                .get("profile_label")
                .and_then(serde_json::Value::as_str)
                .filter(|label| !label.is_empty()),
        })
    }
}

/// Which archived installs may be opened on **this** machine, keyed by install
/// id.
///
/// A record earns a launch target one of two ways, and both are statements about
/// the whole set rather than about the one record:
///
/// · its profile label exactly and uniquely matches one profile's browser-owned
///   name (or its directory name), and no other install of that browser on this
///   machine claims the same label;
/// · the label decides nothing — absent, claimed twice, or matching no profile or
///   more than one — and this machine holds exactly one install for that browser
///   while exactly one local profile has the extension installed, so that profile
///   is the only place this install can be. This is the path an unnamed install
///   takes; without it the one action the topology permits would be unavailable
///   for every install whose reader never typed a name.
///
/// Anything else is ambiguous and gets no target, which is what the row then
/// says: an unknown profile is never opened as some profile.
pub fn discover(
    installs: &[serde_json::Value],
    local_machine: Option<&str>,
) -> BTreeMap<String, OpenTarget> {
    let Some(local_machine) = local_machine else {
        return BTreeMap::new();
    };
    match_installs(installs, local_machine, &local_profiles())
}

/// The matching rule, read off profile metadata this caller supplies. Separate
/// from [`discover`] so it is the same code on every platform, and the same code
/// a test can hand any candidate set — including the ambiguous ones the real
/// browser on this machine happens not to have.
fn match_installs(
    installs: &[serde_json::Value],
    local_machine: &str,
    local_states: &BTreeMap<String, ProfileMap>,
) -> BTreeMap<String, OpenTarget> {
    let local = installs
        .iter()
        .filter_map(|install| LocalInstall::of(install, local_machine))
        .collect::<Vec<_>>();

    // Two facts that only the whole set can settle. How many installs each
    // browser holds here, counted by install id so that one report reaching this
    // list through two destinations is still one install — and a record whose id
    // could not be read is counted as `None`, because as far as this machine can
    // tell it is another install, and a uniqueness it cannot prove may not open a
    // profile. Which labels more than one install claims: two installs under one
    // name is one profile directory that both rows would open, and which record
    // is the one living there is not knowable from here.
    let mut ids_per_browser: BTreeMap<&'static str, BTreeSet<Option<&str>>> = BTreeMap::new();
    let mut label_claims: BTreeMap<(&'static str, &str), BTreeSet<Option<&str>>> = BTreeMap::new();
    for this in &local {
        ids_per_browser
            .entry(this.browser_id)
            .or_default()
            .insert(this.install_id);
        if let Some(label) = this.profile_label {
            label_claims
                .entry((this.browser_id, label))
                .or_default()
                .insert(this.install_id);
        }
    }

    let mut found = BTreeMap::new();
    for this in &local {
        // A record with no readable install id has no key to file a target
        // under, so the row cannot be linked to one — and it was counted above
        // all the same.
        let Some(install_id) = this.install_id else {
            continue;
        };
        let Some(profiles) = local_states.get(this.browser_id) else {
            continue;
        };
        let exact = this
            .profile_label
            .filter(|label| {
                label_claims
                    .get(&(this.browser_id, *label))
                    .is_none_or(|claims| claims.len() < 2)
            })
            .and_then(|label| unique_profile_match(profiles, label));
        let unique_installed = ids_per_browser
            .get(this.browser_id)
            .is_some_and(|ids| ids.len() == 1)
            .then(|| unique_extension_profile(profiles))
            .flatten();
        let Some(profile_directory) = exact.or(unique_installed) else {
            continue;
        };
        found.insert(
            install_id.to_owned(),
            OpenTarget {
                browser_id: this.browser_id.to_owned(),
                profile_directory,
            },
        );
    }
    found
}

/// This machine's browser profile metadata, keyed by browser id.
///
/// Empty anywhere the profile layout is not read on: with no candidate profiles
/// there is nothing to match, so every install keeps its `no match` row rather
/// than a guessed one.
fn local_profiles() -> BTreeMap<String, ProfileMap> {
    #[cfg(not(target_os = "macos"))]
    {
        BTreeMap::new()
    }

    #[cfg(target_os = "macos")]
    {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return BTreeMap::new();
        };
        let user_data_root = home.join("Library/Application Support");
        let mut local_states = BTreeMap::new();
        for browser in crate::nativehost::Browser::ALL {
            let Some(target) = crate::nativehost::target(
                crate::nativehost::Platform::Macos,
                &user_data_root,
                browser,
                crate::nativehost::HOST_NAME,
            ) else {
                continue;
            };
            let Some(profile_root) = target.profile_root else {
                continue;
            };
            if let Some(profiles) = read_profiles(&profile_root) {
                local_states.insert(browser.id().to_owned(), profiles);
            }
        }
        local_states
    }
}

fn browser_id(label: &str) -> Option<&'static str> {
    match label.to_ascii_lowercase().as_str() {
        "arc" => Some("arc"),
        "brave" => Some("brave"),
        "chrome" => Some("chrome"),
        "chromium" => Some("chromium"),
        "edge" => Some("edge"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn read_profiles(root: &Path) -> Option<BTreeMap<String, ProfileCandidate>> {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("Local State")).ok()?).ok()?;
    let cache = value.get("profile")?.get("info_cache")?.as_object()?;
    let mut profiles = BTreeMap::new();
    for (directory, info) in cache {
        if !valid_profile_directory(directory) {
            continue;
        }
        let Some(name) = info.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let prefs: serde_json::Value = std::fs::read(root.join(directory).join("Preferences"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::Value::Null);
        let has_extension = prefs
            .get("extensions")
            .and_then(|extensions| extensions.get("settings"))
            .and_then(|settings| settings.get(crate::nativehost::CHROME_EXTENSION_ID))
            .is_some();
        profiles.insert(
            directory.clone(),
            ProfileCandidate {
                directory: directory.clone(),
                name: name.to_owned(),
                has_extension,
            },
        );
    }
    Some(profiles)
}

fn valid_profile_directory(directory: &str) -> bool {
    directory == "Default"
        || directory
            .strip_prefix("Profile ")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn unique_profile_match(
    profiles: &BTreeMap<String, ProfileCandidate>,
    label: &str,
) -> Option<String> {
    let mut matches = profiles
        .iter()
        .filter(|(directory, profile)| profile.name == label || directory.as_str() == label)
        .map(|(_, profile)| profile.directory.clone());
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

fn unique_extension_profile(profiles: &BTreeMap<String, ProfileCandidate>) -> Option<String> {
    let mut matches = profiles
        .values()
        .filter(|profile| profile.has_extension)
        .map(|profile| profile.directory.clone());
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

/// Open the known extension coverage page in the verified profile.
pub fn launch(target: &OpenTarget) -> Result<(), &'static str> {
    let Some(application) = target.application() else {
        return Err("This browser does not have a verified local launcher.");
    };
    #[cfg(target_os = "macos")]
    {
        let profile_arg = format!("--profile-directory={}", target.profile_directory);
        let extension_url = format!(
            "chrome-extension://{}/coverage.html",
            crate::nativehost::CHROME_EXTENSION_ID
        );
        let status = std::process::Command::new("/usr/bin/open")
            .arg("-a")
            .arg(application)
            .arg("--args")
            .arg(profile_arg)
            .arg(extension_url)
            .status()
            .map_err(|_| "The browser could not be started.")?;
        if status.success() {
            Ok(())
        } else {
            Err("The browser did not open the requested profile.")
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = application;
        Err("Exact profile opening is unavailable on this operating system.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_match_is_exact_unique_and_directory_bounded() {
        let profiles = BTreeMap::from([
            (
                "Default".to_owned(),
                ProfileCandidate {
                    directory: "Default".to_owned(),
                    name: "Personal".to_owned(),
                    has_extension: true,
                },
            ),
            (
                "Profile 2".to_owned(),
                ProfileCandidate {
                    directory: "Profile 2".to_owned(),
                    name: "Work".to_owned(),
                    has_extension: false,
                },
            ),
            (
                "Profile 3".to_owned(),
                ProfileCandidate {
                    directory: "Profile 3".to_owned(),
                    name: "Work".to_owned(),
                    has_extension: false,
                },
            ),
            (
                "../../evil".to_owned(),
                ProfileCandidate {
                    directory: "../../evil".to_owned(),
                    name: "Other".to_owned(),
                    has_extension: false,
                },
            ),
        ]);
        assert_eq!(
            unique_profile_match(&profiles, "Personal").as_deref(),
            Some("Default")
        );
        assert_eq!(unique_profile_match(&profiles, "Work"), None);
        assert_eq!(unique_profile_match(&profiles, "personal"), None);
        assert_eq!(
            unique_extension_profile(&profiles).as_deref(),
            Some("Default")
        );
        assert!(!valid_profile_directory("../../evil"));
    }

    fn candidate(directory: &str, name: &str, has_extension: bool) -> (String, ProfileCandidate) {
        (
            directory.to_owned(),
            ProfileCandidate {
                directory: directory.to_owned(),
                name: name.to_owned(),
                has_extension,
            },
        )
    }

    /// One browser's profile metadata, as this machine's browser would have
    /// written it: directory name, display name, extension present or not.
    fn local_states(
        browser_id: &str,
        profiles: &[(&str, &str, bool)],
    ) -> BTreeMap<String, ProfileMap> {
        BTreeMap::from([(
            browser_id.to_owned(),
            profiles
                .iter()
                .map(|(d, n, e)| candidate(d, n, *e))
                .collect(),
        )])
    }

    /// One archived install record, carrying only the fields the rule reads.
    /// `None` for the id is a record whose id could not be read, and `None` for
    /// the label covers both `null` and an absent field — an install nobody has
    /// named.
    fn record(
        install_id: Option<&str>,
        machine: &str,
        browser: &str,
        label: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "install_id": install_id,
            "machine": machine,
            "browser": browser,
            "profile_label": label,
        })
    }

    fn targets(entries: &[(&str, &str, &str)]) -> BTreeMap<String, OpenTarget> {
        entries
            .iter()
            .map(|(install_id, browser_id, directory)| {
                (
                    (*install_id).to_owned(),
                    OpenTarget {
                        browser_id: (*browser_id).to_owned(),
                        profile_directory: (*directory).to_owned(),
                    },
                )
            })
            .collect()
    }

    /// The audit's case: an install nobody has named is not an install with no
    /// action. One install for this browser on this machine, and one profile
    /// carrying the extension, leaves exactly one place it can be.
    #[test]
    fn an_unnamed_install_opens_the_one_profile_that_holds_the_extension() {
        let installs = [record(Some("synthetic-a"), "this-machine", "Chrome", None)];
        let states = local_states(
            "chrome",
            &[("Default", "Personal", true), ("Profile 2", "Work", false)],
        );
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-a", "chrome", "Default")])
        );
    }

    /// Two unnamed installs of one browser is the shape the page cannot resolve:
    /// which of the two sits in the profile with the extension is not knowable
    /// from here, so neither gets an action rather than one getting a guess.
    #[test]
    fn two_unnamed_installs_of_one_browser_are_left_without_an_action() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", None),
            record(Some("synthetic-b"), "this-machine", "Chrome", None),
        ];
        let states = local_states(
            "chrome",
            &[("Default", "Personal", true), ("Profile 2", "Work", false)],
        );
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            BTreeMap::new()
        );
    }

    /// The other half of the same ambiguity: the archive says one install, the
    /// browser metadata says the extension sits in two profiles. An unnamed
    /// install with two candidate profiles is still unnamed.
    #[test]
    fn the_fallback_never_chooses_between_two_profiles_holding_the_extension() {
        let installs = [record(Some("synthetic-a"), "this-machine", "Chrome", None)];
        let states = local_states(
            "chrome",
            &[("Default", "Personal", true), ("Profile 2", "Work", true)],
        );
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            BTreeMap::new()
        );
    }

    /// Two installs claiming one name is one profile directory that both rows
    /// would open, and which record actually lives there is not knowable from
    /// here. Neither may be opened — and the count that forbids the fallback is
    /// what makes that true even if the browser metadata has a single candidate.
    #[test]
    fn a_label_two_installs_claim_opens_nothing_for_either() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", Some("Work")),
            record(Some("synthetic-b"), "this-machine", "Chrome", Some("Work")),
        ];
        let states = local_states("chrome", &[("Default", "Work", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            BTreeMap::new()
        );
    }

    /// The duplicate check is per label, not per browser: two installs that each
    /// name their own profile are two precise matches, whatever the install count
    /// does to the fallback.
    #[test]
    fn distinct_labels_match_their_own_profiles_even_with_two_installs() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", Some("Work")),
            record(
                Some("synthetic-b"),
                "this-machine",
                "Chrome",
                Some("Personal"),
            ),
        ];
        let states = local_states(
            "chrome",
            &[("Default", "Personal", false), ("Profile 2", "Work", false)],
        );
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[
                ("synthetic-a", "chrome", "Profile 2"),
                ("synthetic-b", "chrome", "Default")
            ])
        );
    }

    /// A label that matches nothing is the third way the label decides nothing,
    /// and the fallback covers it too: the profile a reader renamed under the
    /// archive's feet is still the only profile with the extension in it.
    #[test]
    fn an_install_whose_label_matches_no_profile_falls_back_to_the_extension() {
        let installs = [record(
            Some("synthetic-a"),
            "this-machine",
            "Chrome",
            Some("Renamed away"),
        )];
        let states = local_states("chrome", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-a", "chrome", "Default")])
        );
    }

    /// A second record whose install id could not be read is a second install as
    /// far as this machine can tell, and an unproven uniqueness opens nothing.
    /// The unreadable record gets no target either: there is no key to file one
    /// under, which is not the same as it having no install.
    #[test]
    fn a_sibling_record_without_a_readable_install_id_proves_no_uniqueness() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", None),
            record(None, "this-machine", "Chrome", None),
        ];
        let states = local_states("chrome", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            BTreeMap::new()
        );
    }

    /// One install reported into this list through two destinations is one
    /// install. Counting the records instead of the ids would call it two and
    /// take the action away from an install that has only ever existed once.
    #[test]
    fn one_install_reaching_the_list_twice_is_still_one_install() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", None),
            record(Some("synthetic-a"), "this-machine", "Chrome", None),
        ];
        let states = local_states("chrome", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-a", "chrome", "Default")])
        );
    }

    /// Another machine's installs are neither candidates nor count toward this
    /// machine's uniqueness: the dashboard here cannot open a profile there, and
    /// what it holds there proves nothing about how many Chrome installs are on
    /// this machine.
    #[test]
    fn a_record_from_another_machine_neither_opens_nor_counts() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Chrome", None),
            record(Some("synthetic-there"), "other-machine", "Chrome", None),
        ];
        let states = local_states("chrome", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-a", "chrome", "Default")])
        );
    }

    /// A browser this machine has no profile metadata for gets no target, and the
    /// install of a browser it does have is not affected by that absence.
    #[test]
    fn a_browser_without_local_profile_metadata_opens_nothing() {
        let installs = [
            record(Some("synthetic-a"), "this-machine", "Edge", None),
            record(Some("synthetic-b"), "this-machine", "Chrome", None),
        ];
        let states = local_states("chrome", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-b", "chrome", "Default")])
        );
    }

    /// The launch target travels with the browser it was matched in, so a row for
    /// one browser cannot be launched as another.
    #[test]
    fn a_target_names_the_browser_its_record_reported() {
        let installs = [record(Some("synthetic-a"), "this-machine", "Brave", None)];
        let states = local_states("brave", &[("Default", "Personal", true)]);
        assert_eq!(
            match_installs(&installs, "this-machine", &states),
            targets(&[("synthetic-a", "brave", "Default")])
        );
    }
}
