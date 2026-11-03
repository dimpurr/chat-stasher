//! Connection reaping for remote SSH backends.
//!
//! Every opendal SFTP operation opens a *new* ssh ControlPersist master
//! (`-o ControlPersist=yes` is passed explicitly, so `~/.ssh/config` cannot
//! override it) and never reaps it. Left alone, leaked masters pile up until
//! the box refuses new connections, so every run ends by shutting down the
//! masters it created — and only those.
//!
//! Ownership rule (W1080): [`record_startup_snapshot`] captures the `-S`
//! sockets of all live masters once at process startup, before the first
//! remote operation. The end-of-run reap shuts down only sockets that appeared
//! afterwards (set difference). A sibling chat-stasher's long-lived masters
//! predate the snapshot and are naturally preserved: reaping them mid-listing
//! was observed to truncate the sibling's SFTP listing into false "not
//! present" errors (W1079). So a master is only ever touched when (a) it
//! carries a ControlMaster marker `-M`, (b) it has a `-S` socket, (c) it names
//! the target host, and (d) its socket is new since this process started.
//!
//! Residuals, stated plainly: a master created *after* this process's snapshot
//! by a *sibling* process is still reaped (newness is per-socket, not
//! per-owner — that race needs a sibling starting mid-run), and masters
//! leaked by an already-dead process are no longer collected. Both are
//! accepted over the old behaviour: killing a live sibling fails its run
//! loudly, while leftovers accumulate slowly and are visible in `ps`.

use std::process::Command;
use std::sync::OnceLock;

/// The fake target passed to `-O exit`. openssh treats it as an opaque tag
/// and never actually connects (a real host must not be used here).
const EXIT_TAG: &str = "none";

/// `-S` sockets of every live ssh ControlMaster on this machine at process
/// startup (`None` when the snapshot itself failed). Read at reap time; the
/// first recorded snapshot wins so the ownership window never slides forward.
static STARTUP_SOCKETS: OnceLock<Option<Vec<String>>> = OnceLock::new();

/// Capture the startup snapshot for the ownership rule above. Call once at
/// process startup, before the first remote operation. Silent: a failed
/// snapshot is reported at reap time, where the decision not to reap is made.
pub fn record_startup_snapshot() {
    // `set` fails only when a snapshot is already recorded; the first one
    // wins, so a second call can never narrow what this run owns.
    let _first_snapshot_wins = STARTUP_SOCKETS.set(all_master_sockets().ok());
}

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

/// Shut down the ssh ControlPersist masters for `host` that this process
/// created: the live set now minus the startup snapshot. An unavailable
/// process listing is an unknown count, not zero; per-master failures are
/// logged, never fatal. With no usable snapshot, reap nothing rather than
/// risk a sibling's masters — that leaks slowly and says so on stderr.
pub fn reap_masters_for_host(host: &str) -> Result<usize, String> {
    let current = masters_for_host(host)?;
    let Some(before) = STARTUP_SOCKETS.get().and_then(Option::as_ref) else {
        eprintln!(
            "[reap] host {host} · ssh masters shut down: 0 \
             (no startup snapshot, leaving them alone)"
        );
        return Ok(0);
    };
    Ok(reap_sockets(
        &added_sockets(before, &current),
        &mut real_exit,
    ))
}

/// Sockets in `after` that were absent from `before`, in `after` order,
/// de-duplicated. The whole ownership rule in one pure function: everything
/// the tests assert about "mine versus a sibling's" goes through here.
pub fn added_sockets(before: &[String], after: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for sock in after {
        if !before.contains(sock) && !out.contains(sock) {
            out.push(sock.clone());
        }
    }
    out
}

/// What one `ssh -O exit` attempt did. `SpawnFailed` keeps the long-standing
/// behaviour for an unstartable ssh: a one-line note, never counted.
enum ExitResult {
    Exited,
    Failed(String),
    SpawnFailed(String),
}

/// The production executor: `ssh -O exit` against one socket.
fn real_exit(sock: &str) -> ExitResult {
    match Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-S", sock, "-O", "exit", EXIT_TAG])
        .output()
    {
        Ok(out) if out.status.success() => ExitResult::Exited,
        Ok(out) => ExitResult::Failed(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => ExitResult::SpawnFailed(e.to_string()),
    }
}

/// Run `ssh -O exit` over `socks` via `exit`, returning how many masters shut
/// down. A socket that is already gone is silence, not news; anything else
/// failed is one stderr line, never fatal. `exit` is `FnMut` so tests can
/// record every socket they were asked to shut down.
fn reap_sockets(socks: &[String], exit: &mut dyn FnMut(&str) -> ExitResult) -> usize {
    let mut exited = 0;
    for sock in socks {
        match exit(sock) {
            ExitResult::Exited => exited += 1,
            ExitResult::Failed(stderr) => {
                if is_missing_control_socket(&stderr) {
                    // The master already exited and removed its socket;
                    // nothing needs reaping and no user cares.
                    continue;
                }
                eprintln!("reap: `ssh -O exit` failed for {sock}: {}", stderr.trim());
            }
            ExitResult::SpawnFailed(e) => eprintln!("reap: could not run ssh for {sock}: {e}"),
        }
    }
    exited
}

/// Enumerate `-S` socket paths of live ssh masters for `host`.
///
/// Returns an error when `ps` itself is unavailable or unreadable; otherwise a
/// (possibly empty) de-duplicated list of sockets.
fn masters_for_host(host: &str) -> Result<Vec<String>, String> {
    Ok(sockets_in_ps_output(&ps_dump()?, Some(host)))
}

/// Every live ssh master socket on this machine, whatever host it names: the
/// startup snapshot. Same error contract as [`masters_for_host`].
fn all_master_sockets() -> Result<Vec<String>, String> {
    Ok(sockets_in_ps_output(&ps_dump()?, None))
}

/// One `ps -eo pid,command` dump. Split out so both enumerations share the
/// same unavailable-process-listing behaviour.
fn ps_dump() -> Result<String, String> {
    let out = Command::new("ps")
        .args(["-eo", "pid,command"])
        .output()
        .map_err(|e| format!("could not run ps: {e}"))?;
    if !out.status.success() {
        return Err(format!("ps exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse one `ps` dump into de-duplicated `-S` sockets of `ssh -M` masters,
/// for `host`, or for every host when `host` is `None`. Pure: the unit tests
/// drive this on canned dumps and never touch a live `ps`.
fn sockets_in_ps_output(text: &str, host: Option<&str>) -> Vec<String> {
    let mut socks = Vec::new();
    for line in text.lines() {
        for sock in sockets_in_line_opt(line, host) {
            if !socks.contains(&sock) {
                socks.push(sock);
            }
        }
    }
    socks
}

/// Extract `-S` socket paths from one `ps` line if it is the ssh master for
/// `host`. Only processes that (a) spawn the `ssh` binary itself, (b) carry
/// the ControlMaster marker `-M`, and (c) name `host` as an argument are
/// considered. Verified against the live master shape:
/// `ssh -E <log> -S <dir>/master -M -f -N -o ControlPersist=yes ... -p 23 -l u -l <host>`.
fn sockets_in_line(line: &str, host: &str) -> Vec<String> {
    sockets_in_line_opt(line, Some(host))
}

/// [`sockets_in_line`] with the host check optional. `host = None` takes every
/// ssh master line regardless of host: the startup snapshot must not yet know
/// which host this run will dial.
fn sockets_in_line_opt(line: &str, host: Option<&str>) -> Vec<String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    // `ps -o command` output: `<pid> ssh ...` (pid column is right-aligned, so
    // bespoke prefixes (e.g. `sftp-server`) compare fine here).
    if toks.len() < 2 || toks[1] != "ssh" {
        return Vec::new();
    }
    if !toks.iter().any(|t| *t == "-M") {
        return Vec::new();
    }
    if let Some(host) = host {
        let names_host = toks
            .iter()
            .any(|t| *t == host || t.strip_suffix(':') == Some(host));
        if !names_host {
            return Vec::new();
        }
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

    #[test]
    fn added_sockets_is_after_minus_before_deduped() {
        let before = vec!["/tmp/a/master".to_string(), "/tmp/b/master".to_string()];
        let after = vec![
            "/tmp/b/master".to_string(),
            "/tmp/c/master".to_string(),
            "/tmp/c/master".to_string(),
        ];
        assert_eq!(
            added_sockets(&before, &after),
            vec!["/tmp/c/master".to_string()]
        );
        assert_eq!(added_sockets(&before, &before), Vec::<String>::new());
        assert_eq!(added_sockets(&[], &before), before);
    }

    /// W1080 core, hermetic: process A owns a master before this run starts;
    /// the end-of-run reap must preserve A's master and still shut down this
    /// run's own master. Both `ps` dumps are canned text and `ssh -O exit` is
    /// a recording fake — no real ssh ever runs.
    #[test]
    fn reap_preserves_sibling_master_and_reaps_own() {
        let host = "u000000.your-storagebox.example";
        let sock_a = "/tmp/w1080-fakeA/master";
        let sock_b = "/tmp/w1080-fakeB/master";
        let before_ps = format!(
            "  111 ssh -S {sock_a} -M -f -N -o ControlPersist=yes -p 23 -l u000000 {host}\n"
        );
        let mut after_ps = before_ps.clone();
        after_ps.push_str(&format!(
            "  222 ssh -S {sock_b} -M -f -N -o ControlPersist=yes -p 23 -l u000000 {host}\n"
        ));
        let before = sockets_in_ps_output(&before_ps, None);
        assert_eq!(before, vec![sock_a.to_string()]);
        let current = sockets_in_ps_output(&after_ps, Some(host));
        assert_eq!(current.len(), 2);
        let mut exited: Vec<String> = Vec::new();
        let n = reap_sockets(&added_sockets(&before, &current), &mut |sock| {
            exited.push(sock.to_string());
            ExitResult::Exited
        });
        assert_eq!(n, 1);
        assert_eq!(exited, vec![sock_b.to_string()]);
        assert!(!exited.iter().any(|s| s == sock_a));
    }

    /// The guard against a fix that reaps nothing: with an empty startup (the
    /// common case — no masters alive when the process starts), this run's own
    /// masters are still shut down.
    #[test]
    fn reap_with_empty_startup_still_reaps_own_masters() {
        let host = "u000000.your-storagebox.example";
        let sock_b = "/tmp/w1080-fakeB/master";
        let before_ps = "  999 /sbin/launchd\n";
        let after_ps = format!(
            "  999 /sbin/launchd\n  222 ssh -S {sock_b} -M -f -N -o ControlPersist=yes -p 23 -l u000000 {host}\n"
        );
        let before = sockets_in_ps_output(before_ps, None);
        assert!(before.is_empty());
        let current = sockets_in_ps_output(&after_ps, Some(host));
        let mut exited: Vec<String> = Vec::new();
        let n = reap_sockets(&added_sockets(&before, &current), &mut |sock| {
            exited.push(sock.to_string());
            ExitResult::Exited
        });
        assert_eq!(n, 1);
        assert_eq!(exited, vec![sock_b.to_string()]);
    }
}
