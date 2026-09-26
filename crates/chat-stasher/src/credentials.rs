//! Persistent credential references for backend option values.
//!
//! A destination option (for example `access_key_id`) may be written as a
//! reference instead of a literal secret. The historical form, `env:NAME`, is
//! resolved out of the **process environment** by `config`; it works from an
//! interactive shell, but a process a scheduler started (launchd / systemd) or
//! a GUI app has no login shell and therefore no such variable. This module
//! owns the forms that resolve **without a login shell**:
//!
//! * `file:PATH` — the secret is the file's contents, with one trailing
//!   newline trimmed.
//! * `env-file:PATH:NAME` — the secret is the `NAME=` entry of a dotenv-style
//!   file (`NAME=value` lines, `#` comments, an optional `export ` prefix, and
//!   optional matching single/double quotes around the value).
//! * `keychain:ACCOUNT` (or `keychain:SERVICE:ACCOUNT`) — the secret is a
//!   generic-password item read with macOS `security find-generic-password -w`;
//!   the service defaults to `chat-stasher`.
//!
//! Resolution is **fail-closed**: a reference that cannot be used is an error
//! naming the option and the missing credential — never a silently omitted
//! option and never a substituted empty string. The literal and `env:NAME`
//! forms keep their own documented behaviour (`docs-dev/install.md` §4.5).
//!
//! The resolved secret is returned to the caller and lives only in the
//! in-memory config; it is never logged. Only the reference's *name* (a path,
//! variable or keychain item) appears in an error, because a reference that a
//! user mistyped may itself be a secret they pasted where a name belonged.

use std::path::PathBuf;
use std::process::Command;

/// The default keychain service for a `keychain:ACCOUNT` reference.
pub const DEFAULT_KEYCHAIN_SERVICE: &str = "chat-stasher";

/// Environment variable that overrides the `security` binary, so a test (or a
/// wrapper) can point the lookup at a shim instead of the real keychain. Same
/// shape as `CHAT_STASHER_LAUNCHCTL` / `CHAT_STASHER_SYSTEMCTL`.
pub const SECURITY_TOOL_ENV: &str = "CHAT_STASHER_SECURITY";

/// What [`resolve`] found in an option value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Not one of this module's reference forms; the caller leaves it alone.
    NotReference,
    /// The reference resolved to this secret.
    Value(String),
    /// The reference is one of ours but cannot be used. The reason names the
    /// missing credential and never carries a value.
    Missing(String),
}

/// Resolve one option value written as a persistent credential reference.
pub fn resolve(value: &str) -> Resolved {
    if let Some(rest) = value.strip_prefix("file:") {
        return resolve_file(rest);
    }
    if let Some(rest) = value.strip_prefix("env-file:") {
        return resolve_env_file(rest);
    }
    if let Some(rest) = value.strip_prefix("keychain:") {
        return resolve_keychain(rest);
    }
    Resolved::NotReference
}

/// `file:PATH` — read the whole file as the secret.
fn resolve_file(path: &str) -> Resolved {
    if path.is_empty() {
        return Resolved::Missing("credential reference `file:` names no path".to_string());
    }
    let expanded = match crate::config::expand_and_verify(path) {
        Ok(path) => path,
        Err(e) => {
            return Resolved::Missing(format!(
                "credential file path `{path}` could not be resolved: {e}"
            ))
        }
    };
    match std::fs::read_to_string(&expanded) {
        Ok(text) => Resolved::Value(trim_trailing_newline(&text)),
        Err(e) => Resolved::Missing(format!(
            "credential file {} could not be read: {e}",
            expanded.display()
        )),
    }
}

/// `env-file:PATH:NAME` — read the `NAME` entry of a dotenv-style file.
///
/// The split is on the **last** colon so a Windows drive letter
/// (`env-file:C:\secrets.env:KEY`) keeps its colon in the path.
fn resolve_env_file(rest: &str) -> Resolved {
    let Some((path, name)) = rest.rsplit_once(':') else {
        return Resolved::Missing(format!(
            "credential reference `env-file:{rest}` must be written `env-file:PATH:NAME`"
        ));
    };
    if path.is_empty() || name.is_empty() {
        return Resolved::Missing(format!(
            "credential reference `env-file:{rest}` must name both a file and a variable"
        ));
    }
    let expanded = match crate::config::expand_and_verify(path) {
        Ok(path) => path,
        Err(e) => {
            return Resolved::Missing(format!("env file path `{path}` could not be resolved: {e}"))
        }
    };
    match std::fs::read_to_string(&expanded) {
        Ok(text) => match parse_env_file(&text, name) {
            Some(secret) => Resolved::Value(secret),
            None => Resolved::Missing(format!(
                "env file {} has no `{name}` entry",
                expanded.display()
            )),
        },
        Err(e) => Resolved::Missing(format!(
            "env file {} could not be read: {e}",
            expanded.display()
        )),
    }
}

/// `keychain:ACCOUNT` / `keychain:SERVICE:ACCOUNT` — read a generic-password
/// item with the macOS `security` tool.
///
/// Deliberately not behind a `cfg(target_os = "macos")`: on a platform with no
/// `security` tool the lookup fails with a reason that names the tool, which is
/// the same honest answer as a missing keychain item and keeps the branch
/// testable by shimming the tool on `PATH`. Only the service and account are
/// passed as arguments; the secret comes back on the tool's stdout and is not
/// echoed anywhere.
fn resolve_keychain(rest: &str) -> Resolved {
    if rest.is_empty() {
        return Resolved::Missing("credential reference `keychain:` names no account".to_string());
    }
    let (service, account) = match rest.split_once(':') {
        Some((service, account)) if !service.is_empty() && !account.is_empty() => {
            (service.to_string(), account.to_string())
        }
        Some(_) => {
            return Resolved::Missing(format!(
                "credential reference `keychain:{rest}` must be `keychain:ACCOUNT` or \
                 `keychain:SERVICE:ACCOUNT`"
            ))
        }
        None => (DEFAULT_KEYCHAIN_SERVICE.to_string(), rest.to_string()),
    };
    let tool = std::env::var_os(SECURITY_TOOL_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("security"));
    let output = Command::new(&tool)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
        ])
        .output();
    match output {
        Err(e) => Resolved::Missing(format!(
            "keychain item service={service:?} account={account:?} could not be read: the \
             `security` tool could not be run ({e})"
        )),
        Ok(output) if output.status.success() => match String::from_utf8(output.stdout) {
            Ok(secret) => {
                let secret = trim_trailing_newline(&secret);
                if secret.is_empty() {
                    Resolved::Missing(format!(
                        "keychain item service={service:?} account={account:?} is set but empty"
                    ))
                } else {
                    Resolved::Value(secret)
                }
            }
            Err(_) => Resolved::Missing(format!(
                "keychain item service={service:?} account={account:?} is not valid UTF-8"
            )),
        },
        Ok(_) => Resolved::Missing(format!(
            "no keychain item service={service:?} account={account:?}; store one with \
             `security add-generic-password -s {service} -a {account} -w`"
        )),
    }
}

/// Strip exactly one trailing `\n` (and a `\r` before it), so a secret written
/// by `echo` or a text editor is not compared with a stray newline.
fn trim_trailing_newline(text: &str) -> String {
    text.strip_suffix('\n')
        .map(|rest| rest.strip_suffix('\r').unwrap_or(rest))
        .unwrap_or(text)
        .to_string()
}

/// The value of `name` in a dotenv-style file, if present. First match wins.
fn parse_env_file(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == name {
            return Some(strip_quotes(value.trim()).to_string());
        }
    }
    None
}

/// Remove one pair of matching surrounding quotes. A value that does not begin
/// and end with the same quote character is returned unchanged.
fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// `SECURITY_TOOL_ENV` is process-global and these tests run as threads of
    /// one process, so the two that set it take turns.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ordinary_values_and_env_refs_are_left_alone() {
        assert_eq!(resolve("literal-secret"), Resolved::NotReference);
        assert_eq!(resolve("env:CHAT_STASHER_KEY"), Resolved::NotReference);
        assert_eq!(resolve("opendal:s3"), Resolved::NotReference);
    }

    #[test]
    fn file_reference_reads_contents_and_trims_one_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secret");
        fs::write(&path, "s3cr3t\n").unwrap();
        assert_eq!(
            resolve(&format!("file:{}", path.display())),
            Resolved::Value("s3cr3t".to_string())
        );
    }

    #[test]
    fn file_reference_missing_file_names_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nope");
        match resolve(&format!("file:{}", path.display())) {
            Resolved::Missing(why) => assert!(
                why.contains("could not be read"),
                "the reason must say the file could not be read: {why}"
            ),
            other => panic!("expected Missing, got {other:?}"),
        }
        assert!(matches!(resolve("file:"), Resolved::Missing(_)));
    }

    #[test]
    fn env_file_reference_parses_dotenv_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secrets.env");
        fs::write(
            &path,
            "# a comment\n\nOTHER=1\nexport CHAT_STASHER_KEY=\"quoted value\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve(&format!("env-file:{}:CHAT_STASHER_KEY", path.display())),
            Resolved::Value("quoted value".to_string())
        );
        match resolve(&format!("env-file:{}:MISSING", path.display())) {
            Resolved::Missing(why) => assert!(why.contains("no `MISSING` entry"), "{why}"),
            other => panic!("expected Missing, got {other:?}"),
        }
        assert!(matches!(
            resolve("env-file:only-a-path"),
            Resolved::Missing(_)
        ));
    }

    /// The `security` tool is shimmed on the environment override, so the
    /// keychain branch is exercised on any platform without touching a real
    /// keychain (and never with a real secret).
    #[test]
    fn keychain_reference_invokes_the_shim() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let shim = dir.path().join("security");
        fs::write(&shim, "#!/bin/sh\nprintf 'keychain-secret\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&shim).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&shim, perms).unwrap();
        }
        // Serialised by the same idea as the config tests: this is one process.
        let old = std::env::var_os(SECURITY_TOOL_ENV);
        std::env::set_var(SECURITY_TOOL_ENV, &shim);
        let result = resolve("keychain:r2");
        match old {
            Some(value) => std::env::set_var(SECURITY_TOOL_ENV, value),
            None => std::env::remove_var(SECURITY_TOOL_ENV),
        }
        // reason: the shim is the only path this branch can take; reaching
        // `Value` proves the arguments were accepted and the newline trimmed.
        assert_eq!(result, Resolved::Value("keychain-secret".to_string()));
    }

    #[test]
    fn keychain_reference_missing_item_names_service_and_account() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let shim = dir.path().join("security");
        fs::write(&shim, "#!/bin/sh\nexit 44\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&shim).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&shim, perms).unwrap();
        }
        let old = std::env::var_os(SECURITY_TOOL_ENV);
        std::env::set_var(SECURITY_TOOL_ENV, &shim);
        let result = resolve("keychain:chat-stasher:r2");
        match old {
            Some(value) => std::env::set_var(SECURITY_TOOL_ENV, value),
            None => std::env::remove_var(SECURITY_TOOL_ENV),
        }
        match result {
            Resolved::Missing(why) => {
                assert!(why.contains("chat-stasher") && why.contains("r2"), "{why}")
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }
}
