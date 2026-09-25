//! view — an *ephemeral* local HTTP window onto the metadata tier.
//!
//! Product decision this module implements (settled in the B5 interface round):
//! no desktop app, no TUI, no resident local service. Reading the archive means
//! `chat-stasher ui`, which binds a short-lived loopback socket, opens a
//! browser, and exits. 99% of the time there is no process and no memory.
//!
//! Three properties are load-bearing, and each is here for a reason:
//!
//! * **Metadata tier, with exactly one exception, and it is announced.** The
//!   dashboard and every list are rendered from one
//!   [`crate::search::search_sessions`] report taken *before* the socket is
//!   bound, so no ordinary request touches the repository. The exception is
//!   `/content`, which fetches and decrypts one session's shards — reached only
//!   by an explicit click on a page that printed the cost first, and backed by a
//!   [`ContentSource`] a test can stub to prove no other route reaches it. That
//!   narrowing is deliberate: the previous version of this module could say "no
//!   request can fetch payload" because it had no content feature at all, and a
//!   claim that broad stops being checkable the moment one is added.
//! * **Loopback is not a security boundary.** Every other program running as
//!   any user on this machine can connect to `127.0.0.1`. So the server is not
//!   "safe because it is local": it requires a per-launch random token, carried
//!   in the URL, and rejects everything else. The token is generated from the
//!   OS CSPRNG on each launch and is never written to a file or a log.
//! * **Only `GET`.** Nothing this server does mutates the archive, so every
//!   other method is a flat rejection rather than a route that happens not to
//!   exist.
//!
//! Privacy line, same as `search`/`read`: machine partition, first 8 hex of the
//! session id, shard counts, byte lengths, unix timestamps. Never payload, never
//! a full session id, never the repository URL (which carries a real hostname —
//! the page is labelled with the destination *name* the user typed).

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Default idle timeout. The server exits on its own after this long with no
/// request, so a forgotten tab cannot leave a listener behind.
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Largest request head we will read. A local browser sends well under this;
/// anything larger is refused rather than buffered.
const MAX_HEAD_BYTES: usize = 8 * 1024;

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
    pub fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }

    pub fn html(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    pub fn json(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json; charset=utf-8",
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
    data: &crate::ui::UiData,
    content: &dyn crate::ui::ContentSource,
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
    let (path, params) = crate::ui::split_target(target);
    match params
        .iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.as_str())
    {
        Some(t) if ct_eq(t, token) => {}
        _ => {
            return Response::text(
                403,
                "Forbidden",
                "view: missing or wrong token. Every program on this machine can reach 127.0.0.1, \
                 so this server requires the per-launch token printed by `chat-stasher ui`.\n",
            );
        }
    }
    match crate::ui::handle(path, &params, token, data, content) {
        Some(resp) => resp,
        None => Response::text(404, "Not Found", crate::ui::no_route_message()),
    }
}

/// Escape text for both element content and double-quoted attribute values, so
/// one function covers every interpolation a page does.
pub fn esc(s: &str) -> String {
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
    data: &crate::ui::UiData,
    idle: Duration,
    content: &dyn crate::ui::ContentSource,
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
                // The non-blocking mode above is the *listener's*. On macOS and
                // the BSDs `accept()` hands it down to the connection; Linux
                // does not (POSIX says it must not, and Linux's man page calls
                // the BSD behaviour out as the difference). This socket must be
                // blocking: `read_head` bounds its read with `SO_RCVTIMEO`,
                // which a non-blocking socket ignores, so a read with nothing
                // to read yet returns `WouldBlock` at once and the request is
                // misread as malformed — the connection is then dropped with no
                // reply at all. A client is entitled to connect and compose its
                // request after that; the handshake finishes in the listen
                // backlog, so the gap is real and was measured: `w15_ui_test`
                // failed 2 runs in 30 on an empty, zero-byte response.
                //
                // `write_all` needs it too: a page larger than the send buffer
                // would otherwise stop at `WouldBlock` and be silently cut
                // short by the best-effort write below.
                if let Err(e) = stream.set_nonblocking(false) {
                    stats.rejected += 1;
                    eprintln!("ui: accepted a connection but cannot set it blocking: {e}");
                    continue;
                }
                let resp = match read_head(&mut stream) {
                    Ok(Some(h)) => {
                        route(&h.method, &h.target, &h.hosts, token, port, data, content)
                    }
                    Ok(None) => Response::text(400, "Bad Request", "view: malformed request\n"),
                    Err(_) => continue,
                };
                if resp.status == 200 {
                    stats.served += 1;
                } else {
                    stats.rejected += 1;
                }
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "A client may disconnect while its response is written; the server intentionally treats this response as best-effort."
                )]
                let _ = stream.write_all(&resp.to_bytes());
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "A client may disconnect before flush; the server intentionally treats this response as best-effort."
                )]
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
    use crate::ui::{fixture, NoContent};

    /// Every route the dashboard serves, read from the router's own table so
    /// this list cannot drift from what `ui::handle` answers and what the 404
    /// body names.
    use crate::ui::ROUTES;

    /// The port every test pretends the OS handed us.
    const P: u16 = 51234;

    /// A `Host` a real browser would send for our own URL, so that tests about
    /// *other* things are not silently passing on the Host rejection path.
    fn ok_host() -> Vec<String> {
        vec![format!("127.0.0.1:{P}")]
    }

    /// One request, with the payload tier stubbed out.
    fn get(target: &str, token: &str, hosts: &[String]) -> Response {
        route("GET", target, hosts, token, P, &fixture::data(), &NoContent)
    }

    #[test]
    fn token_is_required_on_every_route() {
        let h = ok_host();
        for target in ROUTES {
            assert_eq!(get(target, "wrongtoken", &h).status, 403, "{target}");
            assert_eq!(get(target, "", &h).status, 403, "{target}");
            assert_eq!(
                get(&format!("{target}?token=wrong"), "goodtoken", &h).status,
                403,
                "{target}"
            );
            assert_eq!(
                get(&format!("{target}?token=tokenish"), "goodtoken", &h).status,
                403,
                "{target}"
            );
            // The token must be recognised in any query position, and a valid
            // token must reach the route rather than being rejected here.
            assert_ne!(
                get(&format!("{target}?token=goodtoken"), "goodtoken", &h).status,
                403,
                "{target}: instrument check — a correct token must not be refused"
            );
            assert_ne!(
                get(&format!("{target}?a=1&token=goodtoken"), "goodtoken", &h).status,
                403,
                "{target}: instrument check — the token is found anywhere in the query"
            );
        }
    }

    /// The instrument above only says "not 403"; this says the two routes that
    /// take no parameters actually render.
    #[test]
    fn the_overview_routes_render_with_a_valid_token() {
        let h = ok_host();
        for target in ["/", "/api/overview", "/api/sessions", "/sessions"] {
            let r = get(&format!("{target}?token=t"), "t", &h);
            assert_eq!(r.status, 200, "{target}");
            assert!(
                !r.body.is_empty(),
                "{target}: a 200 with an empty body would pass the status check"
            );
        }
        assert_eq!(
            get("/api/overview?token=t", "t", &h).content_type,
            "application/json; charset=utf-8"
        );
        assert_eq!(
            get("/?token=t", "t", &h).content_type,
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn only_get_is_accepted_even_with_a_valid_token() {
        let h = ok_host();
        for m in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            for target in ROUTES {
                assert_eq!(
                    route(
                        m,
                        &format!("{target}?token=t"),
                        &h,
                        "t",
                        P,
                        &fixture::data(),
                        &NoContent
                    )
                    .status,
                    405,
                    "method {m} on {target}"
                );
            }
        }
        assert_eq!(get("/?token=t", "t", &h).status, 200);
    }

    /// Method must be rejected before Host, so a `POST` from a rebinding page
    /// is still reported as a method error rather than leaking which check
    /// happens to run first.
    #[test]
    fn method_is_checked_before_host() {
        let evil = vec![format!("evil.example:{P}")];
        assert_eq!(
            route(
                "POST",
                "/?token=t",
                &evil,
                "t",
                P,
                &fixture::data(),
                &NoContent
            )
            .status,
            405
        );
    }

    /// An unauthorised caller must not be able to map the route table.
    #[test]
    fn unknown_routes_are_indistinguishable_from_known_ones_without_a_token() {
        let h = ok_host();
        assert_eq!(get("/secret", "t", &h).status, 403);
        assert_eq!(get("/secret?token=t", "t", &h).status, 404);
        for target in ROUTES {
            assert_ne!(
                get(&format!("{target}?token=t"), "t", &h).status,
                404,
                "{target} must be a real route"
            );
        }
    }

    /// DNS rebinding: `evil.com` re-resolves to `127.0.0.1`, and the browser
    /// sends our port with *its* name in `Host`. The token already stops this
    /// from reading anything, but the Host allowlist must stop it too — and it
    /// must stop it even when the token is correct, which is the whole point of
    /// a second lock.
    #[test]
    fn rebinding_hosts_are_rejected_even_with_the_right_token() {
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
            for target in ROUTES {
                assert_eq!(
                    get(&format!("{target}?token=t"), "t", hosts).status,
                    403,
                    "Host {hosts:?} must be refused on {target}"
                );
            }
        }
    }

    /// The negative test above is worthless unless the instrument can say yes.
    #[test]
    fn legitimate_hosts_are_accepted() {
        for host in [
            format!("127.0.0.1:{P}"),
            format!("localhost:{P}"),
            // hostnames are case-insensitive
            format!("LocalHost:{P}"),
        ] {
            let hosts = vec![host.clone()];
            assert_eq!(
                get("/?token=t", "t", &hosts).status,
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
                "ui must bind the IPv4 loopback literal and nothing else"
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

    /// **A connection is not a request.** `accept()` can hand back a client
    /// before one byte of its request has arrived: the handshake completes into
    /// the listen backlog, so the client's `connect()` returns first, and the
    /// server can accept in the gap between that and the client's `write`.
    ///
    /// Nothing in the accept loop may treat "no bytes yet" as "no request".
    /// This test widens that gap deliberately — the request is written 500 ms
    /// after `connect` returns, while the loop accepts every 50 ms, so the
    /// accept lands ten polls before the write — which makes the window
    /// certain rather than occasional. The same window, unfixed, is what made
    /// `w15_ui_test` fail 2 runs in 30, both times as
    /// `tests/w15_ui_test.rs:210`, "a complete response head", on a response
    /// that was zero bytes long.
    #[test]
    fn a_connection_is_answered_even_if_its_request_has_not_arrived_yet() {
        let listener = bind_ephemeral().expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let data = fixture::data();
        let server = std::thread::spawn(move || {
            serve(
                &listener,
                "token",
                &data,
                Duration::from_secs(2),
                &NoContent,
            )
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect loopback");
        // The server accepts this connection during this sleep, with an empty
        // receive buffer and no request in it.
        std::thread::sleep(Duration::from_millis(500));
        write!(
            stream,
            "GET /?token=token HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");

        let mut raw = Vec::new();
        let read = stream.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "a client is entitled to a reply however the connect/write pair \
             interleaves with accept(); got {} bytes ({read:?})",
            raw.len()
        );
        assert!(
            text.contains("\r\n\r\n"),
            "the reply must carry a complete head: {text:?}"
        );
        assert!(text.contains("text/html"), "the overview page: {text:?}");

        // Let the loop time out and stop rather than leaving a listener behind.
        let stats = server
            .join()
            .expect("the serve loop must not panic")
            .expect("the serve loop must exit cleanly");
        assert_eq!(
            stats.served, 1,
            "the one request above must have been served"
        );
    }
}
