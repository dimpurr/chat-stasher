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
    /// Short form of the session id (`id::short_session_id`) — readable head
    /// plus a sha256 tag, never the full id.
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

/// Fill `buf` from the OS CSPRNG.
///
/// Each platform's own facility is called directly rather than adding a
/// `rand`/`getrandom` dependency: one more crate is one more piece of supply
/// chain for 32 bytes. There is deliberately **no** portable fallback branch —
/// a platform we have not wired up must fail to build, not quietly reach for
/// something weaker.
#[cfg(unix)]
fn os_random(buf: &mut [u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

/// Windows has no `/dev/urandom`. `BCryptGenRandom` with the system-preferred
/// RNG is the documented equivalent and needs no crate — CI on `windows-latest`
/// is what actually checks this, since it cannot be exercised here.
#[cfg(windows)]
fn os_random(buf: &mut [u8]) -> std::io::Result<()> {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let len = u32::try_from(buf.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "buffer too large"))?;
    let status = unsafe {
        BCryptGenRandom(
            core::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("BCryptGenRandom failed: 0x{status:08x}"),
        ))
    }
}

/// A per-launch token from the OS CSPRNG.
///
/// A failure is an error, never a weaker fallback — a guessable token is worse
/// than a `view` that refuses to start.
pub fn new_token() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    os_random(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Bind an ephemeral loopback listener.
///
/// `127.0.0.1` and port `0`, both non-negotiable: `0.0.0.0` would expose the
/// archive index to the network, and a fixed port would be squattable by
/// another local program between launches.
///
/// `::1` is deliberately *not* bound as well. It was measured (B52 task 2):
/// a client asked for `http://localhost:<port>` against an IPv4-only listener
/// tries `[::1]` first, takes one `Connection refused`, and falls back to
/// `127.0.0.1` — it connects. And the caller never prints a `localhost` URL
/// anyway: it formats `listener.local_addr()`, which is the literal
/// `127.0.0.1:<port>`, so no name resolution happens at all. A second listener
/// would mean a second socket in a single-threaded accept loop for a problem
/// that does not exist. If the printed URL ever changes to `localhost`, the
/// cost is one wasted failed connection per launch — revisit then.
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

/// Is this request addressed to *us*, under a name that can only mean this
/// machine and this socket?
///
/// **This is defence in depth, not the only defence.** The primary defence is
/// the per-launch token. Worked through: a DNS-rebinding page on `evil.com`
/// re-resolves its own name to `127.0.0.1`, so the browser treats
/// `http://evil.com:<port>/` as same-origin and lets the attacker's JavaScript
/// *read* the response. But reading anything here still needs the token, which
/// is 256 bits of `/dev/urandom` ([`new_token`]) on a port the OS picked at
/// random. So rebinding alone does not get the archive index; without the token
/// it gets a 403.
///
/// It is implemented anyway, because "the token holds" is a claim about things
/// that are one edit away from changing:
///
/// * The token travels in the URL. Today [`render_html`] loads no off-host
///   asset and links nowhere, so it never leaves in a `Referer` — but that is
///   an invariant of the page, not of the server, and pages get edited.
/// * URLs leak by other routes entirely: history sync, a shoulder-surfed
///   terminal, a pasted screenshot, a shell history file.
///
/// In every one of those, this check is the second lock and costs a dozen
/// lines. That trade is why it is here: arguing that a lock is *secondary* is
/// cheap and this comment does it; arguing that a lock is *unnecessary* is a
/// conclusion we cannot afford to have wrong once.
///
/// What it does **not** buy: a rebinding page with no token still learns
/// "something answered on this port" from the 403, so `view`'s presence is not
/// hidden from a local-port scan. That is unchanged, and not claimed.
///
/// Rules, and why each is strict rather than lenient:
/// * Exactly one `Host`. Zero is rejected — HTTP/1.1 requires it, and omitting
///   it is the first thing you try against a Host allowlist. Two or more is
///   rejected rather than resolved, because picking one is what smuggling is for.
/// * The port must be *our* port, matched numerically, so `:{port}extra` and
///   a bare name with no port both fail.
/// * The name must equal `127.0.0.1` or `localhost` outright. Never a suffix or
///   substring match: `localhost.evil.example` and `127.0.0.1.evil.example` are
///   names an attacker can register.
/// * `[::1]` is absent on purpose — [`bind_ephemeral`] does not bind `::1`, so
///   nothing can legitimately arrive under that name. The allowlist lists only
///   what is actually served.
pub fn host_is_local(hosts: &[String], port: u16) -> bool {
    let [host] = hosts else {
        return false;
    };
    let Some((name, given_port)) = host.rsplit_once(':') else {
        return false;
    };
    if given_port.parse::<u16>() != Ok(port) {
        return false;
    }
    name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost")
}

/// Route one request. Pure: no socket, no repository, no clock.
///
/// Order matters. Method is checked before the token so that a `POST` is
/// rejected as a method error even when it carries a valid token; `Host` is
/// checked next, because a request addressed to someone else is not a request
/// to this server at all and should not reach the token comparison; the token
/// is checked before the path so that an unauthorised caller cannot use
/// 404-vs-200 to learn which routes exist. `Host` may safely precede the token
/// because it carries no secret — the attacker chose its value.
pub fn route(
    method: &str,
    target: &str,
    hosts: &[String],
    token: &str,
    port: u16,
    data: &ViewData,
) -> Response {
    if method != "GET" {
        return Response::text(
            405,
            "Method Not Allowed",
            "view: only GET is accepted; there is nothing here to mutate\n",
        );
    }
    if !host_is_local(hosts, port) {
        return Response::text(
            403,
            "Forbidden",
            "view: bad Host header. This server answers only requests addressed to \
             127.0.0.1 or localhost on its own port; a request arriving under another \
             name is a DNS-rebinding attempt. This is a second lock — the token is the \
             first.\n",
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

/// The only three things [`route`] needs off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestHead {
    method: String,
    target: String,
    /// Every `Host` header seen, in order — *not* collapsed to the first.
    /// Two `Host` headers is a request-smuggling smell, and a check that reads
    /// only the first one is exactly what such a request is built to fool.
    hosts: Vec<String>,
}

/// Pull the request line and the `Host` header(s) out of a request head.
/// Split out from [`read_head`] so the parsing is testable without a socket.
fn parse_head(text: &str) -> Option<RequestHead> {
    let mut lines = text.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m.to_string(), t.to_string()),
        _ => return None,
    };
    let mut hosts = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("host") {
                hosts.push(v.trim().to_string());
            }
        }
    }
    Some(RequestHead {
        method,
        target,
        hosts,
    })
}

/// Read the request head (up to the blank line) and parse it.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<RequestHead>> {
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
    Ok(parse_head(&String::from_utf8_lossy(&buf)))
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
    // Read once: the `Host` allowlist is "our own address", and our own port is
    // whatever the OS handed out at bind time.
    let port = listener.local_addr()?.port();
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
                    Ok(Some(h)) => route(&h.method, &h.target, &h.hosts, token, port, data),
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

    /// The port every test pretends the OS handed us.
    const P: u16 = 51234;

    /// A `Host` a real browser would send for our own URL, so that tests about
    /// *other* things are not silently passing on the Host rejection path.
    fn ok_host() -> Vec<String> {
        vec![format!("127.0.0.1:{P}")]
    }

    #[test]
    fn token_is_required_on_every_route() {
        let d = data();
        let h = ok_host();
        for target in ["/", "/api/sessions"] {
            assert_eq!(route("GET", target, &h, "goodtoken", P, &d).status, 403);
            assert_eq!(
                route(
                    "GET",
                    &format!("{target}?token=wrong"),
                    &h,
                    "goodtoken",
                    P,
                    &d
                )
                .status,
                403
            );
            assert_eq!(
                route(
                    "GET",
                    &format!("{target}?token=goodtoken"),
                    &h,
                    "goodtoken",
                    P,
                    &d
                )
                .status,
                200,
                "instrument check: the correct token must actually pass"
            );
        }
    }

    #[test]
    fn only_get_is_accepted_even_with_a_valid_token() {
        let d = data();
        let h = ok_host();
        for m in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            assert_eq!(
                route(m, "/?token=t", &h, "t", P, &d).status,
                405,
                "method {m}"
            );
        }
        assert_eq!(route("GET", "/?token=t", &h, "t", P, &d).status, 200);
    }

    /// Method must be rejected before Host, so a `POST` from a rebinding page
    /// is still reported as a method error rather than leaking which check
    /// happens to run first.
    #[test]
    fn method_is_checked_before_host() {
        let d = data();
        let evil = vec![format!("evil.example:{P}")];
        assert_eq!(route("POST", "/?token=t", &evil, "t", P, &d).status, 405);
    }

    /// An unauthorised caller must not be able to map the route table.
    #[test]
    fn unknown_routes_are_indistinguishable_from_known_ones_without_a_token() {
        let d = data();
        let h = ok_host();
        assert_eq!(route("GET", "/secret", &h, "t", P, &d).status, 403);
        assert_eq!(route("GET", "/secret?token=t", &h, "t", P, &d).status, 404);
    }

    /// DNS rebinding: `evil.com` re-resolves to `127.0.0.1`, and the browser
    /// sends our port with *its* name in `Host`. The token already stops this
    /// from reading anything, but the Host allowlist must stop it too — and it
    /// must stop it even when the token is correct, which is the whole point of
    /// a second lock.
    #[test]
    fn rebinding_hosts_are_rejected_even_with_the_right_token() {
        let d = data();
        let cases: Vec<Vec<String>> = vec![
            // no Host at all — what you send to slip past a Host allowlist
            vec![],
            // attacker-controlled name, our port
            vec![format!("evil.example:{P}")],
            // a name that merely contains an allowed one
            vec![format!("127.0.0.1.evil.example:{P}")],
            vec![format!("localhost.evil.example:{P}")],
            vec![format!("evil.example.localhost:{P}")],
            // right name, wrong port — not the socket we are serving
            vec![format!("127.0.0.1:{}", P + 1)],
            // no port: cannot be our OS-assigned ephemeral port
            vec!["127.0.0.1".to_string()],
            vec!["localhost".to_string()],
            // ::1 is not bound (see bind_ephemeral), so it is not on the list
            vec![format!("[::1]:{P}")],
            // two Host headers: a check reading only the first would pass this
            vec![format!("127.0.0.1:{P}"), format!("evil.example:{P}")],
            vec![format!("evil.example:{P}"), format!("127.0.0.1:{P}")],
            // garbage
            vec![String::new()],
            vec![format!("127.0.0.1:{P}extra")],
        ];
        for hosts in &cases {
            assert_eq!(
                route("GET", "/?token=t", hosts, "t", P, &d).status,
                403,
                "Host {hosts:?} must be refused"
            );
            assert_eq!(
                route("GET", "/api/sessions?token=t", hosts, "t", P, &d).status,
                403,
                "Host {hosts:?} must be refused on the JSON route too"
            );
        }
    }

    /// The negative test above is worthless unless the instrument can say yes.
    #[test]
    fn legitimate_hosts_are_accepted() {
        let d = data();
        for host in [
            format!("127.0.0.1:{P}"),
            format!("localhost:{P}"),
            // hostnames are case-insensitive
            format!("LocalHost:{P}"),
        ] {
            let hosts = vec![host.clone()];
            assert_eq!(
                route("GET", "/?token=t", &hosts, "t", P, &d).status,
                200,
                "instrument check: Host {host} must actually pass"
            );
        }
    }

    #[test]
    fn host_header_is_parsed_case_insensitively_and_not_collapsed() {
        let h = parse_head("GET /?token=x HTTP/1.1\r\nhost: 127.0.0.1:9\r\nAccept: */*\r\n\r\n")
            .expect("well-formed head");
        assert_eq!(h.method, "GET");
        assert_eq!(h.target, "/?token=x");
        assert_eq!(h.hosts, vec!["127.0.0.1:9".to_string()]);

        let dup = parse_head("GET / HTTP/1.1\r\nHost: a:1\r\nHOST: b:1\r\n\r\n").expect("head");
        assert_eq!(
            dup.hosts,
            vec!["a:1".to_string(), "b:1".to_string()],
            "both Host headers must survive parsing so the check can refuse them"
        );

        // Headers after the blank line are body, not headers.
        let body = parse_head("GET / HTTP/1.1\r\nHost: a:1\r\n\r\nHost: b:1\r\n").expect("head");
        assert_eq!(body.hosts, vec!["a:1".to_string()]);

        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("GET\r\n\r\n"), None);
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

    /// A non-loopback bind must never happen. `is_loopback()` alone would still
    /// pass if someone "fixed" a bug by switching to `0.0.0.0` and then to some
    /// other address, so this pins the exact string and rejects the two
    /// mistakes that would actually expose the archive index to the network:
    /// the unspecified address, and a port that is not the OS's to give.
    ///
    /// It also pins IPv4: `::1` is intentionally not bound (B52 task 2 —
    /// `curl http://localhost:<port>` falls back to IPv4, and the printed URL
    /// is the `127.0.0.1` literal). If this assertion is ever relaxed to allow
    /// a second socket, the `Host` allowlist in [`host_is_local`] has to grow
    /// `[::1]` in the same commit.
    #[test]
    fn bind_is_never_non_loopback() {
        for _ in 0..8 {
            let l = bind_ephemeral().expect("bind loopback");
            let addr = l.local_addr().unwrap();
            assert_eq!(
                addr.ip().to_string(),
                "127.0.0.1",
                "view must bind the IPv4 loopback literal and nothing else"
            );
            assert!(addr.is_ipv4(), "::1 is not bound; see bind_ephemeral");
            match addr.ip() {
                std::net::IpAddr::V4(v4) => {
                    assert!(!v4.is_unspecified(), "0.0.0.0 would expose the index");
                    assert!(!v4.is_private() && !v4.is_multicast() && !v4.is_broadcast());
                }
                std::net::IpAddr::V6(_) => unreachable!("asserted ipv4 above"),
            }
            assert_ne!(addr.port(), 0, "port must be assigned by the OS");
        }
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
