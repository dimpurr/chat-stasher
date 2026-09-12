//! ADR-023: Unified error classifier and pre-connection host trust for remote destinations.
//!
//! When connecting to remote storage (e.g. SFTP via openssh), connection failures
//! fall into four primary categories:
//! 1. Host untrusted (strict host key checking fails on first contact).
//! 2. Host fingerprint changed (potential Man-in-the-Middle attack; never auto-trust).
//! 3. Authentication denied (bad key, bad permissions, missing authorized_keys).
//! 4. Remote unreachable (DNS, timeout, connection refused).
//!
//! Any unrecognised failure is passed through verbatim without losing context.

use crate::store::StoreConfig;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Classification of remote connection and transport failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteErrorKind {
    /// Host fingerprint changed: openssh REMOTE HOST IDENTIFICATION HAS CHANGED (potential MITM attack).
    HostKeyChanged,
    /// Host key not yet trusted in ~/.ssh/known_hosts under strict checking.
    HostUntrusted,
    /// Authentication denied by remote host (key rejected, permission denied).
    AuthDenied,
    /// Remote host unreachable (DNS resolution failure, timeout, connection refused, network down).
    Unreachable,
}

impl RemoteErrorKind {
    /// Label for summary reports and diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            RemoteErrorKind::HostKeyChanged => "host fingerprint changed (potential MITM attack)",
            RemoteErrorKind::HostUntrusted => "host untrusted (strict checking)",
            RemoteErrorKind::AuthDenied => "authentication denied",
            RemoteErrorKind::Unreachable => "remote host unreachable",
        }
    }

    /// Stable machine-readable token for `--json` output.
    ///
    /// Separate from [`label`](Self::label) on purpose: the label is prose and
    /// may be reworded, while this is a key that scripts match on, so rewording
    /// the label must not silently change what a script reads.
    pub fn slug(&self) -> &'static str {
        match self {
            RemoteErrorKind::HostKeyChanged => "host_key_changed",
            RemoteErrorKind::HostUntrusted => "host_untrusted",
            RemoteErrorKind::AuthDenied => "auth_denied",
            RemoteErrorKind::Unreachable => "unreachable",
        }
    }
}

/// Parse host and port from an endpoint string.
///
/// Supported formats:
/// - `ssh://user@host:port/path`
/// - `sftp://user@host:port/path`
/// - `user@host:port`
/// - `user@host`
/// - `[ipv6]:port`
/// - `[ipv6]`
/// - `host:port`
/// - `host`
pub fn endpoint_host_port(endpoint: &str) -> (Option<String>, u16) {
    let s = endpoint
        .strip_prefix("ssh://")
        .or_else(|| endpoint.strip_prefix("sftp://"))
        .unwrap_or(endpoint);
    let no_path = s.split('/').next().unwrap_or(s);
    let no_user = no_path.rsplit('@').next().unwrap_or(no_path);
    if no_user.is_empty() {
        return (None, 22);
    }
    // Check IPv6 bracketed: [::1]:23 or [::1]
    if let Some(rest) = no_user.strip_prefix('[') {
        if let Some((ipv6, after_bracket)) = rest.split_once(']') {
            let port = if let Some(port_str) = after_bracket.strip_prefix(':') {
                port_str.parse::<u16>().unwrap_or(22) // reason: port parse fallback to default SSH port 22
            } else {
                22
            };
            return (Some(ipv6.to_string()), port);
        }
    }
    // Host:port
    if let Some((host, port_str)) = no_user.split_once(':') {
        if !host.is_empty() {
            let port = port_str.parse::<u16>().unwrap_or(22); // reason: port parse fallback to default SSH port 22
            return (Some(host.to_string()), port);
        }
    }
    (Some(no_user.to_string()), 22)
}

/// Classify an error string into one of the known remote failure categories.
pub fn classify_error_str(err: &str) -> Option<RemoteErrorKind> {
    // 🔴 Host key changed MUST be checked before HostUntrusted, because
    // OpenSSH's "REMOTE HOST IDENTIFICATION HAS CHANGED" warning also contains
    // the generic "Host key verification failed." line. Collapsing the two
    // would dangerously downgrade a potential MITM security alarm into a routine
    // trust prompt.
    if err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || err.contains("IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY")
        || err.contains("POSSIBLE DNS SPOOFING DETECTED")
        || err.contains("host key has just been changed")
    {
        return Some(RemoteErrorKind::HostKeyChanged);
    }

    // Host untrusted
    if err.contains("host key is known for")
        || err.contains("Host key verification failed")
        || err.contains("StrictHostKeyChecking")
        || err.contains("authenticity of host")
        || err.contains("can't be established")
    {
        return Some(RemoteErrorKind::HostUntrusted);
    }

    // Authentication denied
    if err.contains("Permission denied (")
        || err.contains("Permission denied, please try again")
        || err.contains("Authentication failed")
        || err.contains("Authentication rejected")
    {
        return Some(RemoteErrorKind::AuthDenied);
    }

    // Unreachable (DNS / timeout / connection refused)
    if err.contains("Connection refused")
        || err.contains("Could not resolve hostname")
        || err.contains("Name or service not known")
        || err.contains("Connection timed out")
        || err.contains("Operation timed out")
        || err.contains("Network is unreachable")
        || err.contains("No route to host")
        || err.contains("Connection closed by remote host")
        || err.contains("Connection reset by peer")
    {
        return Some(RemoteErrorKind::Unreachable);
    }

    None
}

/// Format a remote error with actionable advice while preserving the entire raw error chain.
pub fn format_remote_error(prefix: &str, err: &anyhow::Error, cfg: &StoreConfig) -> String {
    let err_str = format!("{err:#}");
    let Some(kind) = classify_error_str(&err_str) else {
        // Classification failed: pass the original error through verbatim rather
        // than swallowing it behind a generic guess.
        return format!("{prefix}: {err_str}");
    };

    let endpoint = cfg
        .options
        .get("endpoint")
        .map(|s| s.as_str())
        .unwrap_or(""); // reason: empty fallback when no endpoint configured in options
    let (parsed_host, port) = endpoint_host_port(endpoint);
    let host = parsed_host.as_deref().unwrap_or("<host>"); // reason: fallback placeholder when host cannot be determined
    let key_file = cfg
        .options
        .get("key")
        .map(|s| s.as_str())
        .unwrap_or("<unspecified-key>"); // reason: placeholder when key not set in options

    let advice_lines: Vec<String> = match kind {
        RemoteErrorKind::HostKeyChanged => vec![
            "🔴 [SECURITY ALERT] REMOTE HOST IDENTIFICATION HAS CHANGED!".to_string(),
            format!("   Potential Man-in-the-Middle (MITM) attack detected for host `{host}`!"),
            "   The host key received from the server differs from the key in ~/.ssh/known_hosts.".to_string(),
            "   Refusing to connect: host key changes will NEVER be automatically accepted.".to_string(),
            "   Verify the authentic host fingerprint out-of-band with your provider before proceeding.".to_string(),
        ],
        RemoteErrorKind::HostUntrusted => vec![
            format!("remote host `{host}` is not trusted: strict host key verification failed"),
            "   First verify the host fingerprint against your provider's published documentation (e.g. Hetzner documentation).".to_string(),
            "   To see the key the network presents (inspect only; ssh-keyscan is not authenticated):".to_string(),
            format!("     ssh-keyscan -p {port} {host}"),
            "   Once it matches the published fingerprint, record it with `--trust-host`:".to_string(),
            "     chat-stasher dest-init --destination <name> --stage <stage> --trust-host".to_string(),
        ],
        RemoteErrorKind::AuthDenied => vec![
            format!("remote authentication denied by `{host}:{port}`: permission denied"),
            "   Please check:".to_string(),
            format!("   1. Private key file configured at `key = \"{key_file}\"` exists"),
            "   2. Private key permissions are owner read-only (chmod 600)".to_string(),
            "   3. The corresponding public key has been added to the remote ~/.ssh/authorized_keys".to_string(),
        ],
        RemoteErrorKind::Unreachable => vec![
            format!("cannot reach remote host `{host}:{port}`"),
            "   Please check your network connection, DNS resolution, and verify the remote port is accessible.".to_string(),
        ],
    };

    // The raw error chain stays in the output: advice first, original text after it.
    let mut out = String::new();
    for line in advice_lines {
        out.push_str(&format!("{prefix}: {line}\n"));
    }
    out.push_str(&format!("{prefix}: original error: {err_str}"));
    out
}

/// Print formatted remote error to standard error.
pub fn eprint_remote_error(prefix: &str, err: &anyhow::Error, cfg: &StoreConfig) {
    eprintln!("{}", format_remote_error(prefix, err, cfg));
}

/// Default path for ~/.ssh/known_hosts, respecting $HOME expansion.
pub fn default_known_hosts_path() -> PathBuf {
    match crate::config::expand_tilde("~/.ssh/known_hosts") {
        Ok(p) => p,
        Err(_) => PathBuf::from(".ssh/known_hosts"),
    }
}

/// Scan host keys using `ssh-keyscan` and compute their fingerprints.
/// Returns pairs of `(raw_known_hosts_line, fingerprint_description)`.
pub fn fetch_host_keys_and_fingerprints(
    host: &str,
    port: u16,
) -> Result<Vec<(String, String)>, String> {
    let port_str = port.to_string();
    let output = Command::new("ssh-keyscan")
        .args(["-p", &port_str, host])
        .output()
        .map_err(|e| format!("failed to execute ssh-keyscan: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Try to compute fingerprint using `ssh-keygen -l -f -`
        let fp = match compute_key_fingerprint(trimmed) {
            Some(f) => f,
            None => {
                // Fallback to key type and host if ssh-keygen is unavailable
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    format!("{} ({})", parts[0], parts[1])
                } else {
                    trimmed.to_string()
                }
            }
        };
        results.push((trimmed.to_string(), fp));
    }

    if results.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim())
        };
        return Err(format!(
            "ssh-keyscan found no host keys for {host}:{port}{detail}"
        ));
    }

    Ok(results)
}

/// Compute fingerprint of a single known_hosts line using `ssh-keygen -l -f -`.
fn compute_key_fingerprint(known_hosts_line: &str) -> Option<String> {
    let mut child = Command::new("ssh-keygen")
        .args(["-l", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let wrote = stdin
            .write_all(known_hosts_line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"));
        // Close the pipe so ssh-keygen sees end of input instead of waiting.
        drop(stdin);
        if wrote.is_err() {
            // ssh-keygen never received a complete key line. Reap the child and
            // report "no fingerprint" — the same answer a failed exit gives
            // below — rather than pretending a write happened.
            child.wait().ok()?;
            return None;
        }
    }

    let output = child.wait_with_output().ok()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Append known_hosts lines to the target known_hosts file, skipping duplicates.
/// Returns the number of newly added lines.
pub fn append_known_hosts(path: &Path, lines: &[String]) -> Result<usize, String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create directory {}: {e}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // The directory holds host keys, so its mode is part of the
                // promise. If it cannot be set, say so instead of continuing
                // with a directory whose mode we do not know.
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .map_err(|e| format!("cannot set mode 0700 on {}: {e}", parent.display()))?;
            }
        }
    }

    let existing = if path.exists() {
        fs::read_to_string(path)
            .map_err(|e| format!("cannot read existing {}: {e}", path.display()))?
    } else {
        String::new()
    };

    let mut existing_set: std::collections::BTreeSet<String> = existing
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let mut added = 0;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if existing_set.insert(trimmed.to_string()) {
            writeln!(file, "{trimmed}")
                .map_err(|e| format!("cannot write to {}: {e}", path.display()))?;
            added += 1;
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Same reason as the directory above: a known_hosts that anything else
        // can edit is not a trust anchor. A failure here is reported, not
        // swallowed, even though the lines are already on disk.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot set mode 0600 on {}: {e}", path.display()))?;
    }

    Ok(added)
}

/// Probe connectivity to a destination repository without reading or modifying archive data.
/// Returns Ok(true) if repository exists, Ok(false) if connected but repo not yet created,
/// or Err on connection/transport error.
pub fn probe_destination_connectivity(cfg: &StoreConfig) -> anyhow::Result<bool> {
    let store = crate::store::BackupStore::for_metadata_query(cfg.clone());
    store.repository_exists()
}

/// Probe a remote destination with error classification.
pub fn probe_classified_destination(
    cfg: &StoreConfig,
) -> Result<bool, (Option<RemoteErrorKind>, String, anyhow::Error)> {
    match probe_destination_connectivity(cfg) {
        Ok(exists) => Ok(exists),
        Err(e) => {
            let err_str = format!("{e:#}");
            let kind = classify_error_str(&err_str);
            let formatted = format_remote_error("doctor", &e, cfg);
            Err((kind, formatted, e))
        }
    }
}

/// Whether a destination names a network backend rather than a local path.
pub fn is_remote_endpoint(cfg: &StoreConfig) -> bool {
    cfg.repo_root.starts_with("opendal:") || cfg.repo_root.starts_with("rest:")
}

/// `(host, port)` of a destination, taken from its `endpoint` option.
///
/// `None` when the destination is local or carries no parseable endpoint —
/// callers must treat that as "no host to talk about", never as port 22 on a
/// guessed host.
pub fn remote_endpoint_host_port(cfg: &StoreConfig) -> Option<(String, u16)> {
    let endpoint = cfg.options.get("endpoint")?;
    let (host, port) = endpoint_host_port(endpoint);
    host.map(|h| (h, port))
}

/// Outcome of the ADR-023 pre-flight probe.
///
/// Two states, deliberately not three: the probe's *failure* is already printed
/// and classified by [`preflight`], and the caller only has to pick an exit
/// code. `Reached` carries the tri-state's remaining distinction — the host
/// answered, and either a repository is there or it is not — as a plain `bool`,
/// because both of those are answers rather than unknowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preflight {
    /// The destination answered. `repository_exists` is that answer.
    Reached { repository_exists: bool },
    /// The destination did not answer; the reason has been printed.
    Unreachable,
}

/// Connect once to a destination before the caller commits to expensive work.
///
/// Read-only: it lists config files and never creates, writes or repairs
/// anything, so a pre-flight can never be the reason a repository appears.
pub fn preflight(cfg: &StoreConfig, prefix: &str) -> Preflight {
    match probe_destination_connectivity(cfg) {
        Ok(exists) => Preflight::Reached {
            repository_exists: exists,
        },
        Err(e) => {
            eprint_remote_error(prefix, &e, cfg);
            if classify_error_str(&format!("{e:#}")) == Some(RemoteErrorKind::HostUntrusted) {
                print_first_connection_guidance(cfg, prefix);
            }
            Preflight::Unreachable
        }
    }
}

/// Print the fingerprint `ssh-keyscan` reports right now, plus the command that
/// records it after the operator has checked it.
///
/// The scan itself is unauthenticated: it is worth printing precisely because
/// the operator has to compare it against a value obtained another way. It is
/// never treated as proof, and nothing here writes to `known_hosts`.
fn print_first_connection_guidance(cfg: &StoreConfig, prefix: &str) {
    let Some((host, port)) = remote_endpoint_host_port(cfg) else {
        eprintln!(
            "{prefix}: this destination has no parseable `endpoint`, so the host whose key is \
             missing cannot be named."
        );
        return;
    };
    eprintln!(
        "{prefix}: the host key of `{host}:{port}` is not in ~/.ssh/known_hosts. Checking it is a \
         step a human has to take: compare the fingerprint below with the one your provider \
         publishes (Hetzner publishes theirs in the Storage Box documentation)."
    );
    match fetch_host_keys_and_fingerprints(&host, port) {
        Ok(keys) => {
            for (_, fingerprint) in &keys {
                eprintln!("{prefix}:   {fingerprint}");
            }
        }
        Err(e) => eprintln!(
            "{prefix}: could not read the host key to show a fingerprint, so there is nothing to \
             compare yet: {e}"
        ),
    }
    eprintln!("{prefix}: ssh-keyscan is not authenticated — the value above only means something once it matches the published one.");
    eprintln!("{prefix}: to record it after checking, re-run the same command with --trust-host:");
    eprintln!(
        "{prefix}:   chat-stasher dest-init --destination <name> --stage <stage> --trust-host"
    );
}

/// What `--trust-host` recorded, for the caller to print.
#[derive(Debug, Clone)]
pub struct TrustHostOutcome {
    pub known_hosts: PathBuf,
    pub host: String,
    pub port: u16,
    /// Host key records the scan returned.
    pub scanned: usize,
    /// Records actually appended — already-present lines are not rewritten.
    pub added: usize,
}

/// ADR-023 `--trust-host`: display the host keys, then append them to
/// `known_hosts`.
///
/// Only ever called when the operator passed the flag explicitly; nothing else
/// in this program writes to `known_hosts`, so a first connection cannot become
/// trusted as a side effect of an unattended run.
pub fn trust_host(cfg: &StoreConfig, known_hosts: &Path) -> Result<TrustHostOutcome, String> {
    let (host, port) = remote_endpoint_host_port(cfg).ok_or_else(|| {
        "no `endpoint` configured for this destination, so there is no host key to trust"
            .to_string()
    })?;

    let keys = fetch_host_keys_and_fingerprints(&host, port)?;
    println!("[trust-host] host           : {host}:{port}");
    println!("[trust-host] known_hosts    : {}", known_hosts.display());
    println!("[trust-host] fingerprints   :");
    for (_, fingerprint) in &keys {
        println!("[trust-host]   {fingerprint}");
    }
    println!(
        "[trust-host] compare the fingerprints above with the ones your provider publishes before \
         this file is used for a real connection."
    );

    let lines: Vec<String> = keys.into_iter().map(|(line, _)| line).collect();
    let scanned = lines.len();
    let added = append_known_hosts(known_hosts, &lines)?;

    println!("[trust-host] records written:");
    for line in &lines {
        println!("[trust-host]   {line}");
    }
    Ok(TrustHostOutcome {
        known_hosts: known_hosts.to_path_buf(),
        host,
        port,
        scanned,
        added,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_classify_host_untrusted_ed25519() {
        let raw = "No ED25519 host key is known for [host]:23 and you have requested strict checking. Host key verification failed.";
        assert_eq!(
            classify_error_str(raw),
            Some(RemoteErrorKind::HostUntrusted)
        );
    }

    #[test]
    fn test_classify_host_untrusted_rsa_multiline() {
        let raw = "No RSA host key is known for example.com and you have requested strict checking.\nHost key verification failed.";
        assert_eq!(
            classify_error_str(raw),
            Some(RemoteErrorKind::HostUntrusted)
        );
    }

    #[test]
    fn test_classify_host_untrusted_strict_checking() {
        let raw = "StrictHostKeyChecking=yes was specified but host is not in known_hosts file";
        assert_eq!(
            classify_error_str(raw),
            Some(RemoteErrorKind::HostUntrusted)
        );
    }

    #[test]
    fn test_classify_host_key_changed_mitm_alarm() {
        let raw = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                   @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                   @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                   IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
                   Someone could be eavesdropping on you right now (man-in-the-middle attack)!\n\
                   Host key verification failed.";
        // Critical: must be HostKeyChanged, NEVER downgraded to HostUntrusted despite containing "Host key verification failed"
        assert_eq!(
            classify_error_str(raw),
            Some(RemoteErrorKind::HostKeyChanged)
        );
    }

    #[test]
    fn test_classify_auth_denied() {
        let raw1 = "Permission denied (publickey,password).";
        assert_eq!(classify_error_str(raw1), Some(RemoteErrorKind::AuthDenied));

        let raw2 = "Permission denied (publickey).";
        assert_eq!(classify_error_str(raw2), Some(RemoteErrorKind::AuthDenied));

        let raw3 = "Authentication failed.";
        assert_eq!(classify_error_str(raw3), Some(RemoteErrorKind::AuthDenied));
    }

    #[test]
    fn test_classify_unreachable() {
        let raw1 = "ssh: connect to host example.com port 22: Connection refused";
        assert_eq!(classify_error_str(raw1), Some(RemoteErrorKind::Unreachable));

        let raw2 = "ssh: Could not resolve hostname example.com: nodename nor servname provided, or not known";
        assert_eq!(classify_error_str(raw2), Some(RemoteErrorKind::Unreachable));

        let raw3 = "ssh: connect to host example.com port 23: Operation timed out";
        assert_eq!(classify_error_str(raw3), Some(RemoteErrorKind::Unreachable));

        let raw4 = "Network is unreachable";
        assert_eq!(classify_error_str(raw4), Some(RemoteErrorKind::Unreachable));
    }

    #[test]
    fn test_classify_unclassified_fallback() {
        let raw = "unexpected filesystem I/O error: disk full";
        assert_eq!(classify_error_str(raw), None);
    }

    #[test]
    fn test_format_remote_error_preserves_error_chain_and_advice() {
        let mut opts = BTreeMap::new();
        opts.insert(
            "endpoint".to_string(),
            "u123456.your-storagebox.example:23".to_string(),
        );
        opts.insert("key".to_string(), "~/.ssh/id_ed25519".to_string());
        let cfg = StoreConfig {
            repo_root: "opendal:sftp".to_string(),
            options: opts,
            ..Default::default()
        };

        // 1. Host untrusted: advice first, keyscan command with port and host, original error second
        let err1 = anyhow::anyhow!("No ED25519 host key is known for [u123456.your-storagebox.example]:23 and you have requested strict checking. Host key verification failed.");
        let out1 = format_remote_error("push", &err1, &cfg);
        assert!(out1.contains("push: remote host `u123456.your-storagebox.example` is not trusted"));
        assert!(out1.contains("ssh-keyscan -p 23 u123456.your-storagebox.example"));
        assert!(out1.contains("push: original error: No ED25519 host key is known"));

        // 2. MITM alarm: security alert first, original error second
        let err2 = anyhow::anyhow!(
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! Host key verification failed."
        );
        let out2 = format_remote_error("verify", &err2, &cfg);
        assert!(out2.contains("🔴 [SECURITY ALERT] REMOTE HOST IDENTIFICATION HAS CHANGED!"));
        assert!(out2
            .contains("verify: original error: WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!"));

        // 3. Auth denied: key path and permissions advice
        let err3 = anyhow::anyhow!("Permission denied (publickey,password).");
        let out3 = format_remote_error("push", &err3, &cfg);
        assert!(out3.contains("push: remote authentication denied by `u123456.your-storagebox.example:23`: permission denied"));
        assert!(out3.contains("~/.ssh/id_ed25519"));
        assert!(out3.contains("chmod 600"));
        assert!(out3.contains("push: original error: Permission denied"));

        // 4. Unreachable
        let err4 = anyhow::anyhow!("Connection refused");
        let out4 = format_remote_error("overview", &err4, &cfg);
        assert!(out4
            .contains("overview: cannot reach remote host `u123456.your-storagebox.example:23`"));
        assert!(out4.contains("overview: original error: Connection refused"));

        // 5. Unclassified fallback: untouched original error
        let err5 = anyhow::anyhow!("custom rustic internal index failure");
        let out5 = format_remote_error("read", &err5, &cfg);
        assert_eq!(out5, "read: custom rustic internal index failure");
    }

    /// The point of a classifier is that different causes come out different.
    /// A function that answered one bucket for everything would satisfy every
    /// single-class test above while being useless, so this pins the whole
    /// partition at once: four real openssh messages, four distinct verdicts,
    /// four distinct `--json` slugs.
    #[test]
    fn test_every_class_gets_a_distinct_kind_and_slug() {
        let cases = [
            (
                "Host untrusted",
                "No ED25519 host key is known for [host]:23 and you have requested strict checking. Host key verification failed.",
                RemoteErrorKind::HostUntrusted,
            ),
            (
                "Host key changed",
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! Host key verification failed.",
                RemoteErrorKind::HostKeyChanged,
            ),
            (
                "Auth denied",
                "Permission denied (publickey,password).",
                RemoteErrorKind::AuthDenied,
            ),
            (
                "Unreachable",
                "ssh: connect to host example.com port 22: Connection refused",
                RemoteErrorKind::Unreachable,
            ),
        ];

        let mut kinds: Vec<RemoteErrorKind> = Vec::new();
        let mut labels: Vec<&'static str> = Vec::new();
        let mut slugs: Vec<&'static str> = Vec::new();
        for (name, raw, expected) in cases {
            let got = classify_error_str(raw);
            assert_eq!(got, Some(expected), "class `{name}` was misclassified");
            let kind = got.unwrap();
            kinds.push(kind);
            labels.push(kind.label());
            slugs.push(kind.slug());
        }

        kinds.sort_by_key(|k| k.slug());
        kinds.dedup();
        assert_eq!(kinds.len(), 4, "two classes collapsed onto one verdict");

        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 4, "two classes share one human label");

        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), 4, "two classes share one --json slug");
    }

    /// A destination's host comes from its `endpoint` option, and an absent or
    /// host-less endpoint must answer `None` rather than a guessed default —
    /// port 22 on an invented host would point the operator at the wrong box.
    #[test]
    fn test_remote_endpoint_host_port_reads_only_a_real_endpoint() {
        let mut opts = BTreeMap::new();
        opts.insert("endpoint".to_string(), "ssh://box.example:23".to_string());
        let cfg = StoreConfig {
            repo_root: "opendal:sftp".to_string(),
            options: opts,
            ..Default::default()
        };
        assert!(is_remote_endpoint(&cfg));
        assert_eq!(
            remote_endpoint_host_port(&cfg),
            Some(("box.example".to_string(), 23))
        );

        // No endpoint option at all.
        let bare = StoreConfig {
            repo_root: "opendal:sftp".to_string(),
            options: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(remote_endpoint_host_port(&bare), None);

        // A local destination is never a "remote endpoint".
        let local = StoreConfig {
            repo_root: "/var/backups/chat".to_string(),
            options: BTreeMap::new(),
            ..Default::default()
        };
        assert!(!is_remote_endpoint(&local));
        assert_eq!(remote_endpoint_host_port(&local), None);
    }

    #[test]
    fn test_endpoint_host_port_parsing() {
        assert_eq!(
            endpoint_host_port("ssh://u123456.your-storagebox.example:23"),
            (Some("u123456.your-storagebox.example".to_string()), 23)
        );
        assert_eq!(
            endpoint_host_port("sftp://user@host.example.com:2222/some/path"),
            (Some("host.example.com".to_string()), 2222)
        );
        assert_eq!(
            endpoint_host_port("user@example.com"),
            (Some("example.com".to_string()), 22)
        );
        assert_eq!(
            endpoint_host_port("example.com:2222"),
            (Some("example.com".to_string()), 2222)
        );
        assert_eq!(
            endpoint_host_port("[2001:db8::1]:23"),
            (Some("2001:db8::1".to_string()), 23)
        );
        assert_eq!(
            endpoint_host_port("[2001:db8::1]"),
            (Some("2001:db8::1".to_string()), 22)
        );
        assert_eq!(endpoint_host_port(""), (None, 22));
    }

    #[test]
    fn test_append_known_hosts_isolated() {
        let temp_dir = tempfile::tempdir().unwrap();
        let known_hosts = temp_dir.path().join(".ssh/known_hosts");

        let line1 = "host.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI1111111111111111111111111111111111111111111".to_string();
        let line2 = "[host.example.com]:23 ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC222222222222222222222222222222222222222".to_string();

        // First write: both lines added
        let added = append_known_hosts(&known_hosts, &[line1.clone(), line2.clone()]).unwrap();
        assert_eq!(added, 2);
        assert!(known_hosts.exists());

        let content = fs::read_to_string(&known_hosts).unwrap();
        assert!(content.contains(&line1));
        assert!(content.contains(&line2));

        // Second write (idempotent): no lines added
        let added2 = append_known_hosts(&known_hosts, &[line1.clone(), line2.clone()]).unwrap();
        assert_eq!(added2, 0);

        // Third write with new line: exactly 1 added
        let line3 = "other.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI3333333333333333333333333333333333333333333".to_string();
        let added3 = append_known_hosts(&known_hosts, &[line1, line3.clone()]).unwrap();
        assert_eq!(added3, 1);

        let content2 = fs::read_to_string(&known_hosts).unwrap();
        assert!(content2.contains(&line3));

        // The file and the `.ssh` directory it lives in are created with the
        // modes openssh itself uses. A group- or world-readable known_hosts is
        // a file openssh may refuse to read, which would silently send the next
        // run back to the untrusted-host failure this feature exists to fix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&known_hosts).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "known_hosts must be owner-only");
            let dir_mode = fs::metadata(known_hosts.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, ".ssh must be owner-only");
        }
    }

    /// `--trust-host` must be able to say "there is no host here" instead of
    /// inventing one: a destination without an `endpoint` has no host key to
    /// record, and the failure has to name that rather than defaulting to
    /// port 22 somewhere.
    #[test]
    fn test_trust_host_refuses_a_destination_without_an_endpoint() {
        let cfg = StoreConfig {
            repo_root: "opendal:sftp".to_string(),
            options: BTreeMap::new(),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".ssh/known_hosts");
        let err = trust_host(&cfg, &target).unwrap_err();
        assert!(
            err.contains("no `endpoint` configured"),
            "unexpected message: {err}"
        );
        assert!(
            !target.exists(),
            "a failed --trust-host must not leave a known_hosts behind"
        );
    }
}
