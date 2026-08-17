//! view — an *ephemeral* local HTTP window onto the metadata tier.
//!
//! Product decision this module implements (settled in the B5 interface round):
//! no desktop app, no TUI, no resident local service. Reading the archive means
//! `chat-stasher view`, which binds a short-lived loopback socket, opens a
//! browser, and exits. 99% of the time there is no process and no memory.
//!
//! Three properties are load-bearing, and each is here for a reason:
//!
//! * **Metadata tier only.** The page is rendered from one
//!   [`crate::search::search_sessions`] report taken *before* the socket is
//!   bound. No request touches the repository, so no request can be made to
//!   fetch payload. The page says "正文未加载" out loud, and prints what a
//!   full-text pass *would* cost ([`crate::search::SearchReport::fulltext_cost`]),
//!   because a full-text feature that ships silently becomes the default
//!   expectation — and its price can turn "看归档" into "下载归档".
//! * **Loopback is not a security boundary.** Every other program running as
//!   any user on this machine can connect to `127.0.0.1`. So the server is not
//!   "safe because it is local": it requires a per-launch random token, carried
//!   in the URL, and rejects everything else. The token is generated from the
//!   OS CSPRNG on each launch and is never written to a file or a log.
//! * **Only `GET`.** There is nothing to mutate, so every other method is a
//!   flat rejection rather than a route that happens not to exist.
//!
//! Privacy line, same as `search`/`read`: machine partition, first 8 hex of the
//! session id, shard counts, byte lengths, unix timestamps. Never payload, never
//! a full session id, never the repository URL (which carries a real hostname —
//! the page is labelled with the destination *name* the user typed).

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::search::{FulltextCost, SearchReport};

/// Default idle timeout. The server exits on its own after this long with no
/// request, so a forgotten tab cannot leave a listener behind.
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Largest request head we will read. A local browser sends well under this;
/// anything larger is refused rather than buffered.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// One row of the session list. Every field is metadata-tier and already
/// truncated for display.
#[derive(Debug, Clone)]
pub struct ViewSession {
    pub machine: String,
    /// First 8 chars of the session id — never the full id.
    pub short_id: String,
    pub shard_count: usize,
    pub bytes: u64,
    pub activity_unix: i64,
}

/// Everything the two routes render, computed once before the socket is bound.
#[derive(Debug, Clone)]
pub struct ViewData {
    /// The destination *name* the user typed. Deliberately not `repo_root`,
    /// which would put a real hostname on a page served over a socket.
    pub destination_label: String,
    pub snapshots_scanned: usize,
    pub snapshots_in_repo: usize,
    pub sessions_seen: usize,
    pub sessions: Vec<ViewSession>,
    /// Number of parts of the destination that could not be read. Non-zero
    /// means the list below is incomplete, and the page must say so.
    pub unreadable: usize,
    pub data_blobs_read: usize,
    pub fulltext_cost: FulltextCost,
}

impl ViewData {
    /// Build from a metadata-tier search report. `label` is the destination name.
    pub fn from_report(report: &SearchReport, label: impl Into<String>) -> Self {
        Self {
            destination_label: label.into(),
            snapshots_scanned: report.snapshots_scanned,
            snapshots_in_repo: report.snapshots_in_repo,
            sessions_seen: report.sessions_seen,
            sessions: report
                .hits
                .iter()
                .map(|h| ViewSession {
                    machine: h.machine.clone(),
                    short_id: h.short_id(),
                    shard_count: h.shard_count,
                    bytes: h.bytes,
                    activity_unix: h.activity_unix,
                })
                .collect(),
            unreadable: report.unreadable.len(),
            data_blobs_read: report.data_blobs_read,
            fulltext_cost: report.fulltext_cost(),
        }
    }

    pub fn complete(&self) -> bool {
        self.unreadable == 0
    }
}

/// A rendered HTTP response. Kept as a value so routing is a pure function and
/// the rejection paths are unit-testable without a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }

    /// Wire form. `Connection: close` because there is no keep-alive state to
    /// manage in a single-threaded ephemeral server.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}

/// A per-launch token from the OS CSPRNG.
///
/// `/dev/urandom` is used directly rather than adding a `rand`/`getrandom`
/// dependency: one more crate is one more piece of supply chain for 32 bytes.
/// A read failure is an error, never a weaker fallback — a guessable token is
/// worse than a `view` that refuses to start.
pub fn new_token() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Bind an ephemeral loopback listener.
///
/// `127.0.0.1` and port `0`, both non-negotiable: `0.0.0.0` would expose the
/// archive index to the network, and a fixed port would be squattable by
/// another local program between launches.
pub fn bind_ephemeral() -> std::io::Result<TcpListener> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    TcpListener::bind(addr)
}

/// Length-checked, data-independent string compare, so a wrong token cannot be
/// narrowed down by timing the rejection.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Split `/path?query` and pull `token` out of the query string.
fn path_and_token(target: &str) -> (&str, Option<&str>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let token = query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token").then_some(v)
    });
    (path, token)
}

/// Route one request. Pure: no socket, no repository, no clock.
///
/// Order matters. Method is checked before the token so that a `POST` is
/// rejected as a method error even when it carries a valid token; the token is
/// checked before the path so that an unauthorised caller cannot use 404-vs-200
/// to learn which routes exist.
pub fn route(method: &str, target: &str, token: &str, data: &ViewData) -> Response {
    if method != "GET" {
        return Response::text(
            405,
            "Method Not Allowed",
            "view: only GET is accepted; there is nothing here to mutate\n",
        );
    }
    let (path, given) = path_and_token(target);
    match given {
        Some(t) if ct_eq(t, token) => {}
        _ => {
            return Response::text(
                403,
                "Forbidden",
                "view: missing or wrong token. Every program on this machine can reach 127.0.0.1, \
                 so this server requires the per-launch token printed by `chat-stasher view`.\n",
            );
        }
    }
    match path {
        "/" => Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: render_html(data),
        },
        "/api/sessions" => Response {
            status: 200,
            reason: "OK",
            content_type: "application/json; charset=utf-8",
            body: render_json(data),
        },
        _ => Response::text(
            404,
            "Not Found",
            "view: no such route (only / and /api/sessions)\n",
        ),
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// One page, no assets, no scripts, no links off-host.
pub fn render_html(data: &ViewData) -> String {
    let c = &data.fulltext_cost;
    let mut rows = String::new();
    for s in &data.sessions {
        rows.push_str(&format!(
            "<tr><td>{}</td><td class=mono>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td></tr>\n",
            esc(&s.machine),
            esc(&s.short_id),
            s.shard_count,
            s.bytes,
            s.activity_unix
        ));
    }
    if data.sessions.is_empty() {
        rows.push_str("<tr><td colspan=5>(no sessions in this destination)</td></tr>\n");
    }
    let completeness = if data.complete() {
        "read in full — every snapshot scanned was readable".to_string()
    } else {
        format!(
            "<b>INCOMPLETE</b> — {} part(s) of this destination could not be read. \
             Sessions missing from this list are UNKNOWN, not absent.",
            data.unreadable
        )
    };
    format!(
        "<!doctype html>
<html lang=en><head><meta charset=utf-8>
<meta name=viewport content=\"width=device-width, initial-scale=1\">
<title>chat-stasher view · {dest}</title>
<style>
 body{{font:14px/1.5 -apple-system,system-ui,sans-serif;margin:2rem auto;max-width:60rem;padding:0 1rem}}
 table{{border-collapse:collapse;width:100%}}
 th,td{{border-bottom:1px solid #ddd;padding:.4rem .6rem;text-align:left}}
 td.n,th.n{{text-align:right;font-variant-numeric:tabular-nums}}
 .mono{{font-family:ui-monospace,Menlo,monospace}}
 .note{{background:#fffbe6;border:1px solid #e6d98a;padding:.8rem 1rem;margin:1rem 0}}
 .warn{{background:#ffecec;border:1px solid #e0a0a0;padding:.8rem 1rem;margin:1rem 0}}
 footer{{color:#666;margin-top:2rem;font-size:12px}}
</style></head><body>
<h1>chat-stasher view</h1>
<p>destination <b>{dest}</b> · snapshots {scanned} scanned / {inrepo} in repo ·
   sessions seen {seen} · data blobs read <b>{blobs}</b></p>
<p>{completeness}</p>
<div class=note>
 <b>正文未加载。</b> This page is rendered from archive <i>metadata</i> only
 (snapshot + index + tree). No conversation payload has been fetched or
 decrypted — that is why <code>data blobs read = {blobs}</code>.<br>
 Loading full text for the {fs} session(s) listed here would mean fetching and
 decrypting <b>{fb} data blob(s)</b> across <b>{fsh} shard(s)</b>,
 <b>{fbytes} plaintext byte(s)</b>. Not implemented, and not performed.
</div>
<div class=warn>
 This server is on <code>127.0.0.1</code>, which is <b>not</b> a security
 boundary: any other program on this machine can connect to it. Access is
 gated only by the random token in this page's URL. Do not share the URL.
 The server exits by itself when idle.
</div>
<table>
<thead><tr><th>machine</th><th>session (first 8)</th><th class=n>shards</th><th class=n>bytes</th><th class=n>activity (unix s)</th></tr></thead>
<tbody>
{rows}</tbody></table>
<footer>Metadata tier only · ephemeral loopback server · same JSON at <code>/api/sessions</code> (token required)</footer>
</body></html>
",
        dest = esc(&data.destination_label),
        scanned = data.snapshots_scanned,
        inrepo = data.snapshots_in_repo,
        seen = data.sessions_seen,
        blobs = data.data_blobs_read,
        completeness = completeness,
        fs = c.sessions,
        fb = c.data_blobs,
        fsh = c.shards,
        fbytes = c.plaintext_bytes,
        rows = rows,
    )
}

/// Same content as the page. `complete` is carried explicitly so a machine
/// consumer cannot mistake a truncated list for an empty destination.
pub fn render_json(data: &ViewData) -> String {
    let c = &data.fulltext_cost;
    let sessions: Vec<serde_json::Value> = data
        .sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "machine": s.machine,
                "session_short_id": s.short_id,
                "shards": s.shard_count,
                "bytes": s.bytes,
                "activity_unix": s.activity_unix,
            })
        })
        .collect();
    let v = serde_json::json!({
        "destination": data.destination_label,
        "tier": "metadata",
        "payload_loaded": false,
        "data_blobs_read": data.data_blobs_read,
        "complete": data.complete(),
        "unreadable_parts": data.unreadable,
        "snapshots_scanned": data.snapshots_scanned,
        "snapshots_in_repo": data.snapshots_in_repo,
        "sessions_seen": data.sessions_seen,
        "sessions_listed": data.sessions.len(),
        "fulltext_cost_if_loaded": {
            "sessions": c.sessions,
            "shards": c.shards,
            "data_blobs": c.data_blobs,
            "plaintext_bytes": c.plaintext_bytes,
            "note": "not implemented, not performed",
        },
        "sessions": sessions,
    });
    let mut s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

/// Read the request head (up to the blank line) and return `(method, target)`.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<(String, String)>> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => Ok(Some((m.to_string(), t.to_string()))),
        _ => Ok(None),
    }
}

/// Outcome of a serve loop, for the caller's summary line. Counts only — a
/// request log would be the one place a token could leak to disk, so there
/// isn't one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeStats {
    pub served: usize,
    pub rejected: usize,
}

/// Single-threaded accept loop that exits after `idle` with no request.
///
/// Non-blocking accept plus a short sleep, rather than a thread and a channel:
/// the whole point of `view` is that it is not a service, and the simplest loop
/// that can time itself out is the one least likely to outlive the browser tab.
/// `Ctrl+C` is handled by the OS default (SIGINT terminates); this loop holds no
/// lock, no temp file and no repository handle, so there is nothing to unwind.
pub fn serve(
    listener: &TcpListener,
    token: &str,
    data: &ViewData,
    idle: Duration,
) -> std::io::Result<ServeStats> {
    listener.set_nonblocking(true)?;
    let mut stats = ServeStats::default();
    let mut last = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                last = Instant::now();
                // Belt and braces: the socket is bound to loopback, so a
                // non-loopback peer should be impossible. If it ever happens,
                // drop it rather than serve it.
                if !peer.ip().is_loopback() {
                    stats.rejected += 1;
                    continue;
                }
                let resp = match read_head(&mut stream) {
                    Ok(Some((method, target))) => route(&method, &target, token, data),
                    Ok(None) => Response::text(400, "Bad Request", "view: malformed request\n"),
                    Err(_) => continue,
                };
                if resp.status == 200 {
                    stats.served += 1;
                } else {
                    stats.rejected += 1;
                }
                let _ = stream.write_all(&resp.to_bytes());
                let _ = stream.flush();
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if last.elapsed() >= idle {
                    return Ok(stats);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Best-effort browser launch. Failure is reported by the caller, not fatal:
/// the URL is already on stdout, and on a headless box there is no browser to
/// launch — which is exactly why `--no-open` exists.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("{cmd} exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ViewData {
        ViewData {
            destination_label: "dest-under-test".into(),
            snapshots_scanned: 1,
            snapshots_in_repo: 1,
            sessions_seen: 2,
            sessions: vec![ViewSession {
                machine: "m-1".into(),
                short_id: "01234567".into(),
                shard_count: 2,
                bytes: 100,
                activity_unix: 42,
            }],
            unreadable: 0,
            data_blobs_read: 0,
            fulltext_cost: FulltextCost {
                sessions: 1,
                shards: 2,
                data_blobs: 2,
                plaintext_bytes: 100,
            },
        }
    }

    #[test]
    fn token_is_required_on_every_route() {
        let d = data();
        for target in ["/", "/api/sessions"] {
            assert_eq!(route("GET", target, "goodtoken", &d).status, 403);
            assert_eq!(
                route("GET", &format!("{target}?token=wrong"), "goodtoken", &d).status,
                403
            );
            assert_eq!(
                route("GET", &format!("{target}?token=goodtoken"), "goodtoken", &d).status,
                200,
                "instrument check: the correct token must actually pass"
            );
        }
    }

    #[test]
    fn only_get_is_accepted_even_with_a_valid_token() {
        let d = data();
        for m in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            assert_eq!(route(m, "/?token=t", "t", &d).status, 405, "method {m}");
        }
        assert_eq!(route("GET", "/?token=t", "t", &d).status, 200);
    }

    /// An unauthorised caller must not be able to map the route table.
    #[test]
    fn unknown_routes_are_indistinguishable_from_known_ones_without_a_token() {
        let d = data();
        assert_eq!(route("GET", "/secret", "t", &d).status, 403);
        assert_eq!(route("GET", "/secret?token=t", "t", &d).status, 404);
    }

    #[test]
    fn page_states_that_payload_is_not_loaded_and_prices_it() {
        let html = render_html(&data());
        assert!(html.contains("正文未加载"));
        assert!(html.contains("data blobs read = 0"));
        assert!(html.contains("2 data blob(s)"), "cost must be on the page");
        assert!(
            html.contains("any other program on this machine can connect to it"),
            "the loopback threat must be stated on the page"
        );
    }

    #[test]
    fn json_carries_completeness_so_partial_is_not_read_as_empty() {
        let mut d = data();
        let full: serde_json::Value = serde_json::from_str(&render_json(&d)).unwrap();
        assert_eq!(full["complete"], serde_json::json!(true));
        assert_eq!(full["payload_loaded"], serde_json::json!(false));
        assert_eq!(full["sessions"].as_array().unwrap().len(), 1);

        d.unreadable = 2;
        let partial: serde_json::Value = serde_json::from_str(&render_json(&d)).unwrap();
        assert_eq!(partial["complete"], serde_json::json!(false));
        assert_eq!(partial["unreadable_parts"], serde_json::json!(2));
    }

    #[test]
    fn html_says_incomplete_when_parts_were_unreadable() {
        let mut d = data();
        d.unreadable = 1;
        assert!(render_html(&d).contains("INCOMPLETE"));
        assert!(!render_html(&data()).contains("INCOMPLETE"));
    }

    #[test]
    fn no_full_session_id_reaches_the_rendered_output() {
        // The renderer only ever sees the already-shortened id; this pins that
        // the row is built from `short_id`, not from a full id smuggled in.
        let d = data();
        assert!(render_html(&d).contains("01234567"));
        assert!(!render_html(&d).contains("0123456789"));
    }

    #[test]
    fn html_escapes_machine_names() {
        let mut d = data();
        d.sessions[0].machine = "<script>x</script>".into();
        let html = render_html(&d);
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn tokens_are_distinct_and_long() {
        let a = new_token().expect("/dev/urandom readable");
        let b = new_token().expect("/dev/urandom readable");
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn bind_is_loopback_with_an_os_assigned_port() {
        let l = bind_ephemeral().expect("bind loopback");
        let addr = l.local_addr().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "must never bind a routable address"
        );
        assert_ne!(addr.port(), 0, "port must be assigned by the OS");
    }

    #[test]
    fn ct_eq_matches_plain_equality() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn token_is_parsed_out_of_any_query_position() {
        assert_eq!(path_and_token("/?token=x"), ("/", Some("x")));
        assert_eq!(
            path_and_token("/api/sessions?a=1&token=x"),
            ("/api/sessions", Some("x"))
        );
        assert_eq!(path_and_token("/"), ("/", None));
        assert_eq!(path_and_token("/?tokenish=x"), ("/", None));
    }
}
