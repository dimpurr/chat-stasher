//! Local-only lookup and launch for a precisely matched Chromium profile.
//!
//! The extension can supply a display label, but it cannot read the browser's
//! profile directory. The dashboard therefore opens a local instance only
//! when either the label uniquely matches Chromium's profile metadata or the
//! installed extension is unique both in the archive and browser metadata. No
//! label-to-directory guess is persisted or sent to another machine.

use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTarget {
    pub(crate) browser_id: String,
    pub(crate) profile_directory: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileCandidate {
    directory: String,
    name: String,
    has_extension: bool,
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

/// Find a launch target only for this machine, on macOS, when a profile's
/// browser-owned display name exactly and uniquely matches the archived label.
pub fn discover(
    installs: &[serde_json::Value],
    local_machine: Option<&str>,
) -> BTreeMap<String, OpenTarget> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (installs, local_machine);
        BTreeMap::new()
    }

    #[cfg(target_os = "macos")]
    {
        let Some(local_machine) = local_machine else {
            return BTreeMap::new();
        };
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

        let mut local_counts = BTreeMap::<String, usize>::new();
        for install in installs {
            if install.get("machine").and_then(serde_json::Value::as_str) != Some(local_machine) {
                continue;
            }
            if let Some(browser) = install
                .get("browser")
                .and_then(serde_json::Value::as_str)
                .and_then(browser_id)
            {
                *local_counts.entry(browser.to_owned()).or_default() += 1;
            }
        }

        let mut found = BTreeMap::new();
        for install in installs {
            if install.get("machine").and_then(serde_json::Value::as_str) != Some(local_machine) {
                continue;
            }
            let (Some(id), Some(browser), Some(label)) = (
                install
                    .get("install_id")
                    .and_then(serde_json::Value::as_str),
                install.get("browser").and_then(serde_json::Value::as_str),
                install
                    .get("profile_label")
                    .and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let Some(browser_id) = browser_id(browser) else {
                continue;
            };
            let Some(profiles) = local_states.get(browser_id) else {
                continue;
            };
            let exact = unique_profile_match(profiles, label);
            let unique_installed = (local_counts.get(browser_id) == Some(&1))
                .then(|| unique_extension_profile(profiles))
                .flatten();
            let Some(profile_directory) = exact.or(unique_installed) else {
                continue;
            };
            found.insert(
                id.to_owned(),
                OpenTarget {
                    browser_id: browser_id.to_owned(),
                    profile_directory,
                },
            );
        }
        found
    }
}

#[cfg(target_os = "macos")]
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
}
