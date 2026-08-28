//! Connection reaping for remote SSH backends.
//!
//! Every opendal SFTP operation opens a *new* ssh ControlPersist master
//! (`-o ControlPersist=yes` is passed explicitly, so `~/.ssh/config` cannot
//! override it) and never reaps it.  This module enumerates the live masters
//! pointing at one specific host from `ps` and shuts each one down with
//! `ssh -o BatchMode=yes -S <sock> -O exit none`.
//!
//! Precision rule: only processes that (a) carry a ControlMaster marker `-M`,
//! (b) have a `-S <sock>` socket, and (c) name the target host as an
//! argument are ever touched — a stray ssh client of another box is never
//! harmed.

use std::process::Command;

/// The fake target passed to `-O exit`. openssh treats it as an opaque tag
/// and never actually connects (a real host must not be used here).
const EXIT_TAG: &str = "none";

/// Parse the destination host out of an opendal `endpoint` value, e.g.
/// `ssh://u000000.your-storagebox.example:23` or `host:23` (also tolerates a
/// `user@` prefix and a `/path` suffix).
pub fn host_of_endpoint(endpoint: &str) -> Option<String> {
    let s = endpoint
        .strip_prefix("ssh://")
        .or_else(|| endpoint.strip_prefix("sftp://"))
        .unwrap_or(endpoint);
    let no_path = s.split('/').next().unwrap_or(s);
    let no_user = no_path.rsplit('@').next().unwrap_or(no_path);
    let host = no_user.split(':').next().unwrap_or(no_user);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Returns true when ssh `-O exit` failed only because the control socket
/// is already gone (the master exited on its own). These are not user-facing
/// errors — telling the user we could not close a socket that no longer
/// exists is noise.
fn is_missing_control_socket(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("control socket connect") && stderr.contains("no such file or directory")
}

/// Find the `-S` socket paths of live ssh ControlMaster masters whose
/// command line names `host`, and shut each one down. An unavailable process
/// listing is an unknown count, not zero; per-master failures are logged,
/// never fatal.
pub fn reap_masters_for_host(host: &str) -> Result<usize, String> {
    let socks = masters_for_host(host)?;
    let mut exited = 0;
    for sock in &socks {
        match Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-S", sock, "-O", "exit", EXIT_TAG])
            .output()
        {
            Ok(out) => {
                if out.status.success() {
                    exited += 1;
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if is_missing_control_socket(&stderr) {
                        // The master already exited and removed its socket;
                        // nothing needs reaping and no user cares.
                        continue;
                    }
                    eprintln!("reap: `-O exit` failed for {}: {}", sock, stderr.trim());
                }
            }
            Err(e) => eprintln!("reap: could not run ssh for {}: {e}", sock),
        }
    }
    Ok(exited)
}

/// Enumerate `-S` socket paths of live ssh masters for `host`.
///
/// Returns an error when `ps` itself is unavailable or unreadable; otherwise a
/// (possibly empty) de-duplicated list of sockets.
fn masters_for_host(host: &str) -> Result<Vec<String>, String> {
    let out = Command::new("ps")
        .args(["-eo", "pid,command"])
        .output()
        .map_err(|e| format!("could not run ps: {e}"))?;
    if !out.status.success() {
        return Err(format!("ps exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut socks = Vec::new();
    for line in text.lines() {
        for sock in sockets_in_line(line, host) {
            if !socks.iter().any(|s: &String| s.as_str() == sock) {
                socks.push(sock);
            }
        }
    }
    Ok(socks)
}

/// Extract `-S` socket paths from one `ps` line if it is the ssh master for
/// `host`. Only processes that (a) spawn the `ssh` binary itself, (b) carry
/// the ControlMaster marker `-M`, and (c) name `host` as an argument are
/// considered. Verified against the live master shape:
/// `ssh -E <log> -S <dir>/master -M -f -N -o ControlPersist=yes ... -p 23 -l u -l <host>`.
fn sockets_in_line(line: &str, host: &str) -> Vec<String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    // `ps -o command` output: `<pid> ssh ...` (pid column is right-aligned, so
    // bespoke prefixes (e.g. `sftp-server`) compare fine here).
    if toks.len() < 2 || toks[1] != "ssh" {
        return Vec::new();
    }
    if !toks.iter().any(|t| *t == "-M") {
        return Vec::new();
    }
    let names_host = toks
        .iter()
        .any(|t| *t == host || t.strip_suffix(':') == Some(host));
    if !names_host {
        return Vec::new();
    }
    let mut socks = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        if toks[i] == "-S" {
            socks.push(toks[i + 1].to_string());
        }
        i += 1;
    }
    socks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_control_socket_is_silently_skipped() {
        assert!(is_missing_control_socket(
            "Control socket connect(/tmp/ssh.sock): No such file or directory"
        ));
        assert!(is_missing_control_socket(
            "reap: -O exit failed: Control socket connect(/var/run/x): No such file or directory"
        ));
    }

    #[test]
    fn real_control_socket_failure_is_not_silenced() {
        assert!(!is_missing_control_socket("Permission denied"));
        assert!(!is_missing_control_socket(
            "Control socket connect(/tmp/ssh.sock): Permission denied"
        ));
        assert!(!is_missing_control_socket(
            "Could not request local forwarding."
        ));
    }

    #[test]
    fn endpoint_host_is_parsed() {
        assert_eq!(
            host_of_endpoint("ssh://u000000.your-storagebox.example:23").as_deref(),
            Some("u000000.your-storagebox.example")
        );
        assert_eq!(
            host_of_endpoint("sftp://user@host.example.com:2222").as_deref(),
            Some("host.example.com")
        );
        assert_eq!(
            host_of_endpoint("host.example.com:23").as_deref(),
            Some("host.example.com")
        );
        assert_eq!(
            host_of_endpoint("ssh://u@h.example:23/sub").as_deref(),
            Some("h.example")
        );
        assert_eq!(host_of_endpoint(""), None);
    }

    #[test]
    fn master_lines_are_recognised() {
        let sock = "/var/folders/x/ssh-zabcd/master";
        // Exactly the live master shape, `ps -o command` style (leading spaces).
        let line = format!(
            "  1234 ssh -E /tmp/x.log -S {sock} -M -f -N -o ControlPersist=yes -p 23 -l u000000 u000000.your-storagebox.example"
        );
        assert_eq![
            sockets_in_line(&line, "u000000.your-storagebox.example"),
            vec![sock.to_string()]
        ];
        // A client (no -M) of the same host is not a master.
        let client = format!(" 5555 ssh -S /tmp/x/client -o ControlPersist=yes -p 23 -l u000000 u000000.your-storagebox.example");
        assert_eq![
            sockets_in_line(&client, "u000000.your-storagebox.example"),
            Vec::<String>::new()
        ];
    }

    #[test]
    fn foreign_or_client_processes_are_ignored() {
        let host = "u000000.your-storagebox.example";
        // master of a DIFFERENT host
        let foreign = " 6666 ssh -S /tmp/y/master -M -f -N -o ControlPersist=yes -p 23 -l u999 u999.other-box.de";
        // non-ssh process that merely mentions the host
        let not_ssh = " 7777 /usr/bin/ssh-copy-id u000000.your-storagebox.example";
        // user@host in the endpoint is not the bare host token
        let user_form = " 8888 ssh -S /tmp/z/master -M -f -N -o ControlPersist=yes u000000@u000000.your-storagebox.example";
        assert_eq![sockets_in_line(foreign, host), Vec::<String>::new()];
        assert_eq![sockets_in_line(not_ssh, host), Vec::<String>::new()];
        assert_eq![sockets_in_line(user_form, host), Vec::<String>::new()];
    }
}
