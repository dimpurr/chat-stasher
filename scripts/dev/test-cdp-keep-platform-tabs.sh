#!/usr/bin/env bash
# test-cdp-keep-platform-tabs.sh — bash tests for
# scripts/dev/cdp-keep-platform-tabs.mjs.
#
# Runs against a mock Chrome DevTools endpoint (a tiny HTTP + WebSocket server
# built into this file), so no real browser is involved. The mock lists one
# chat-stasher service worker and a set of page targets, answers the two
# Runtime.evaluate calls the helper makes (the manifest read that identifies
# our extension, and the cs_backfill_lasttick_v1 storage read that carries the
# leg's state), records every Page.reload with its timestamp, and answers
# PUT /json/new the way Chrome does.
#
# Run from the repository root (or anywhere):
#   bash scripts/dev/test-cdp-keep-platform-tabs.sh

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$here/cdp-keep-platform-tabs.mjs"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/test-keep-tabs.XXXXXX")"
# The mock's pid, written by start_mock and read by cleanup — a file rather
# than a variable because the pid is set in this shell and read in the trap.
MOCK_PIDS_FILE="$SCRATCH/mock-pids"
: > "$MOCK_PIDS_FILE"
cleanup() {
  if [ -s "$MOCK_PIDS_FILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] || continue
      kill "$pid" 2>/dev/null || true
    done < "$MOCK_PIDS_FILE"
  fi
  # Backstop for a mock whose pid never reached the file, matched on the
  # scratch path no other run of this test shares.
  pkill -f "$SCRATCH/cdp-mock.mjs" 2>/dev/null || true
  rm -rf "$SCRATCH"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# The helper is Node or nothing, so a machine without node gets a loud skip
# rather than a silent pass.
if ! command -v node >/dev/null 2>&1; then
  echo "note: node not found; skipping the cdp-keep-platform-tabs cases"
  exit 0
fi

PASS=0
fail() { echo "FAIL: $1" >&2; exit 1; }
note() { echo "ok: $1"; PASS=$((PASS + 1)); }

# Every allowlisted host the helper knows, plus one tab that is not on it.
ALL_ALLOWED="chatgpt.com,chat.openai.com,chat.deepseek.com,gemini.google.com,grok.com,claude.ai,www.perplexity.ai,perplexity.ai,www.kimi.com,kimi.com"

# --- the mock Chrome DevTools endpoint ---------------------------------------
# How long start_mock waits for the mock to report its port, in 50ms steps —
# sized for a loaded CI runner, not an idle dev machine.
MOCK_START_TRIES=600
cat > "$SCRATCH/cdp-mock.mjs" <<'EOF'
// A fake Chrome DevTools endpoint for test-cdp-keep-platform-tabs.sh. Test-only.
import http from 'node:http';
import crypto from 'node:crypto';
import fs from 'node:fs';

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : fallback;
}

// no-http-port | running | none — the leg's state the storage read reports.
const tickMode = arg('--tick', 'no-http-port');
const withSw = arg('--with-sw', '1') === '1';
// Comma-separated hosts to list as open page targets.
const pageHosts = arg('--pages', '').split(',').filter(Boolean);
const recordsFile = arg('--records');
const portFile = arg('--port-file');

const EXT_ID = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const TICKS = {
  'no-http-port': { at: 1, ran: false, reason: 'no-http-port', stopped: 'no-http-port', targets: 4 },
  running: { at: 1, ran: true, reason: 'ran', stopped: null, targets: 4 },
  none: null,
};

const pages = pageHosts.map((host) => ({
  id: `page-${host.replace(/[^a-z0-9]+/g, '-')}`,
  type: 'page',
  url: `https://${host}/`,
  host,
}));

function record(event, detail) {
  if (!recordsFile) return;
  fs.appendFileSync(recordsFile, JSON.stringify({ event, ...detail, at: Date.now() }) + '\n');
}

let server;

function targets() {
  const port = server.address().port;
  const list = [];
  if (withSw) {
    list.push({
      id: 'mock-sw',
      type: 'service_worker',
      url: `chrome-extension://${EXT_ID}/background.js`,
      webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/mock-sw`,
    });
  }
  for (const p of pages) {
    list.push({ ...p, webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/${p.id}` });
  }
  return list;
}

// Minimal unmasked protocol plumbing: the test exchanges one small text frame
// in each direction per connection, so a general implementation would be
// untested weight.
function decodeFrame(buf) {
  const opcode = buf[0] & 0x0f;
  const masked = (buf[1] & 0x80) !== 0;
  let len = buf[1] & 0x7f;
  let offset = 2;
  if (len === 126) { len = buf.readUInt16BE(2); offset = 4; }
  else if (len === 127) { len = Number(buf.readBigUInt64BE(2)); offset = 10; }
  let mask = null;
  if (masked) { mask = buf.subarray(offset, offset + 4); offset += 4; }
  const payload = Buffer.from(buf.subarray(offset, offset + len));
  if (mask) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
  return { opcode, text: payload.toString('utf8') };
}

function encodeText(str) {
  const data = Buffer.from(str, 'utf8');
  if (data.length < 126) return Buffer.concat([Buffer.from([0x81, data.length]), data]);
  const header = Buffer.alloc(4);
  header[0] = 0x81;
  header[1] = 126;
  header.writeUInt16BE(data.length, 2);
  return Buffer.concat([header, data]);
}

server = http.createServer((req, res) => {
  if (req.url === '/json/list') {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(targets()));
    return;
  }
  if (req.url.startsWith('/json/new?')) {
    // Chrome requires PUT here, and the tab's URL is the bare query string
    // (`/json/new?https://chatgpt.com/`), not a named parameter.
    const url = decodeURIComponent(req.url.slice('/json/new?'.length));
    let host = '';
    try { host = new URL(url).host; } catch { /* unparseable: refuse */ }
    if (!host) {
      res.statusCode = 400;
      res.end();
      return;
    }
    const target = {
      id: `page-${host.replace(/[^a-z0-9]+/g, '-')}`,
      type: 'page',
      url: `https://${host}/`,
      host,
    };
    pages.push(target);
    record('open', { host });
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(target));
    return;
  }
  res.statusCode = 404;
  res.end();
});

server.on('upgrade', (req, socket) => {
  const key = String(req.headers['sec-websocket-key'] || '');
  const accept = crypto.createHash('sha1')
    .update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n'
    + 'Upgrade: websocket\r\n'
    + 'Connection: Upgrade\r\n'
    + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  socket.on('data', (buf) => {
    const frame = decodeFrame(buf);
    // Answer the close handshake, so the helper's socket does not stay in
    // CLOSING and keep the process alive after it has printed its result.
    if (frame.opcode === 0x8) {
      socket.write(Buffer.from([0x88, 0x00]));
      socket.end();
      return;
    }
    if (frame.opcode !== 0x1) return;
    let message;
    try { message = JSON.parse(frame.text); } catch { return; }
    const path = String(req.url || '');
    if (path.endsWith('/mock-sw')) {
      // The service worker: the two Runtime.evaluate calls the helper makes.
      const expression = String((message.params && message.params.expression) || '');
      let value = null;
      if (expression.includes('getManifest')) {
        value = JSON.stringify({ name: '__MSG_extName__', dn: 'Chat Stasher', version: '0.2.0.25' });
      } else if (expression.includes('chrome.storage.local.get')) {
        const tick = TICKS[tickMode];
        value = JSON.stringify(tick ? { cs_backfill_lasttick_v1: tick } : {});
      }
      socket.write(encodeText(JSON.stringify({ id: message.id, result: { result: { type: 'string', value } } })));
      return;
    }
    // A page target: Page.reload, recorded so the test can assert which tabs
    // were reloaded and how far apart.
    if (message.method === 'Page.reload') {
      const id = path.split('/').pop();
      const host = pages.find((p) => p.id === id)?.host || id;
      record('reload', { target: id, host });
      socket.write(encodeText(JSON.stringify({ id: message.id, result: {} })));
    }
  });
});

server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  // The caller reads the port from a file, so it never parses a log line.
  if (portFile) fs.writeFileSync(portFile, String(port));
  console.log(`test-cdp-mock: listening on 127.0.0.1:${port} (tick ${tickMode}, sw ${withSw})`);
});
EOF

# start_mock <tick-mode> <with-sw> <pages> — starts the mock in THIS shell and
# prints nothing. The port is read afterwards with mock_port, never returned
# through a command substitution: `$(start_mock …)` runs the function in a
# subshell, so a pid set there is invisible to the parent that has to kill it.
# The pid goes to MOCK_PIDS_FILE, which start_mock writes and cleanup reads.
start_mock() {
  rm -f "$SCRATCH/cdp-port"
  : > "$SCRATCH/records.jsonl"
  node "$SCRATCH/cdp-mock.mjs" --tick "$1" --with-sw "$2" --pages "$3" \
    --records "$SCRATCH/records.jsonl" --port-file "$SCRATCH/cdp-port" >"$SCRATCH/mock.log" 2>&1 &
  mock_pid="$!"
  echo "$mock_pid" >> "$MOCK_PIDS_FILE"
  # Two ways to stop waiting, and the budget is neither of them: a mock that
  # has died cannot come back, so its exit ends the wait at once; a mock that
  # is merely slow gets the budget.
  for _ in $(seq 1 "$MOCK_START_TRIES"); do
    [ -s "$SCRATCH/cdp-port" ] && break
    kill -0 "$mock_pid" 2>/dev/null || break
    sleep 0.05
  done
  if [ ! -s "$SCRATCH/cdp-port" ]; then
    echo "--- the CDP mock's output ---" >&2
    cat "$SCRATCH/mock.log" 2>&1 || true
    echo "------------------------------" >&2
    if kill -0 "$mock_pid" 2>/dev/null; then
      fail "the CDP mock did not report a port within $((MOCK_START_TRIES / 20))s (still running)"
    else
      fail "the CDP mock exited before reporting a port"
    fi
  fi
}
mock_port() { cat "$SCRATCH/cdp-port"; }
stop_mock() {
  mock_pid="$(tail -n 1 "$MOCK_PIDS_FILE")"
  kill "$mock_pid" 2>/dev/null || true
  wait "$mock_pid" 2>/dev/null || true
}

# reload_count <host> — how many times the mock recorded a Page.reload for the
# named host. The records file is empty when the helper touched nothing, and
# "the file is empty" must read as zero, not as a missing measurement.
reload_count() {
  [ -s "$SCRATCH/records.jsonl" ] || { echo 0; return; }
  python3 - "$SCRATCH/records.jsonl" "$1" <<'PY'
import json, sys
records = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
print(sum(1 for r in records if r.get('event') == 'reload' and r.get('host') == sys.argv[2]))
PY
}

# reload_time <host> — the ms timestamp of the first recorded Page.reload for
# the named host, or nothing when there was none.
reload_time() {
  [ -s "$SCRATCH/records.jsonl" ] || return 0
  python3 - "$SCRATCH/records.jsonl" "$1" <<'PY'
import json, sys
records = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
for r in records:
    if r.get('event') == 'reload' and r.get('host') == sys.argv[2]:
        print(r['at'])
        break
PY
}

# open_count <host> — how many times the mock recorded a tab being opened for
# the named host. Same empty-file-means-zero rule as reload_count.
open_count() {
  [ -s "$SCRATCH/records.jsonl" ] || { echo 0; return; }
  python3 - "$SCRATCH/records.jsonl" "$1" <<'PY'
import json, sys
records = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
print(sum(1 for r in records if r.get('event') == 'open' and r.get('host') == sys.argv[2]))
PY
}

# 1. the leg reports no-http-port: every open allowlisted tab is reloaded once,
#    the tab outside the allowlist is left alone, and the pass exits 0.
start_mock no-http-port 1 "$ALL_ALLOWED,example.com"
port="$(mock_port)"
if ! CS_TAB_RELOAD_GAP_MS=50 node "$HELPER" --port "$port" --once >"$SCRATCH/o1" 2>&1; then
  cat "$SCRATCH/o1" >&2
  fail "a no-http-port pass should exit 0"
fi
grep -q "leg reports no-http-port" "$SCRATCH/o1" || fail "the leg's state should be reported"
grep -q "reloaded chatgpt.com" "$SCRATCH/o1" || fail "the chatgpt tab should have been reloaded"
grep -q "reloaded grok.com" "$SCRATCH/o1" || fail "the grok tab should have been reloaded"
[ "$(reload_count chatgpt.com)" = "1" ] || fail "chatgpt should be reloaded exactly once"
[ "$(reload_count grok.com)" = "1" ] || fail "grok should be reloaded exactly once"
[ "$(reload_count example.com)" = "0" ] || fail "a tab outside the allowlist must not be reloaded"
grep -q "platforms with a tab: 7/7, allowed tabs open: 10, untouched page targets: 1" "$SCRATCH/o1" || fail "the counts should be reported (10 tabs across 7 platforms, one tab outside the allowlist)"
stop_mock
note "no-http-port reloads each allowlisted tab once and touches nothing else"

# 2. dry run: the same state, but nothing is opened or reloaded.
start_mock no-http-port 1 "$ALL_ALLOWED,example.com"
port="$(mock_port)"
if ! CS_TAB_RELOAD_GAP_MS=50 node "$HELPER" --port "$port" --once --dry-run >"$SCRATCH/o2" 2>&1; then
  cat "$SCRATCH/o2" >&2
  fail "a dry run should exit 0"
fi
grep -q "dry run — would reload 10 tab(s)" "$SCRATCH/o2" || fail "the dry run should plan the reloads"
[ -s "$SCRATCH/records.jsonl" ] && [ "$(wc -l < "$SCRATCH/records.jsonl" | tr -d ' ')" != "0" ] && fail "a dry run must not reload or open anything"
note "dry run prints the plan and changes nothing"

# 3. a healthy leg: no reloads, exit 0.
start_mock running 1 "$ALL_ALLOWED,example.com"
port="$(mock_port)"
if ! node "$HELPER" --port "$port" --once >"$SCRATCH/o3" 2>&1; then
  cat "$SCRATCH/o3" >&2
  fail "a healthy pass should exit 0"
fi
grep -q "leg reports ok" "$SCRATCH/o3" || fail "the leg's healthy state should be reported"
[ -s "$SCRATCH/records.jsonl" ] && [ "$(wc -l < "$SCRATCH/records.jsonl" | tr -d ' ')" != "0" ] && fail "a healthy leg must not be reloaded"
stop_mock
note "a healthy leg is left alone"

# 4. a missing allowlisted host is opened, and the fresh tab is not reloaded.
start_mock no-http-port 1 "chatgpt.com,example.com"
port="$(mock_port)"
if ! CS_TAB_RELOAD_GAP_MS=50 node "$HELPER" --port "$port" --once >"$SCRATCH/o4" 2>&1; then
  cat "$SCRATCH/o4" >&2
  fail "a pass that opens a missing tab should exit 0"
fi
grep -q "opened https://grok.com/" "$SCRATCH/o4" || fail "the missing host should be opened"
grep -q "reloaded chatgpt.com" "$SCRATCH/o4" || fail "the already-open allowed tab should be reloaded"
[ "$(reload_count grok.com)" = "0" ] || fail "a tab this pass opened is fresh and must not be reloaded"
# chatgpt.com is open, so its redirect spelling must not be opened as a second
# tab for the same platform — the leg needs one channel per platform, and two
# tabs would mean two reloads of the same site every pass.
[ "$(open_count chat.openai.com)" = "0" ] || fail "a platform whose tab is already open must not get a second tab"
[ "$(open_count perplexity.ai)" = "0" ] || fail "only the canonical host of a missing platform is opened"
stop_mock
note "a missing allowlisted host is opened, and the fresh tab is not reloaded"

# 5. reloads are spaced: two allowed tabs, a 400 ms gap, and the mock's own
#    timestamps must show at least that much between the two reloads.
start_mock no-http-port 1 "chatgpt.com,grok.com,example.com"
port="$(mock_port)"
if ! CS_TAB_RELOAD_GAP_MS=400 node "$HELPER" --port "$port" --once >"$SCRATCH/o5" 2>&1; then
  cat "$SCRATCH/o5" >&2
  fail "a spaced pass should exit 0"
fi
first="$(reload_time chatgpt.com)"
second="$(reload_time grok.com)"
[ -n "$first" ] && [ -n "$second" ] || fail "both allowed tabs should have been reloaded"
gap=$((second - first))
[ "$gap" -ge 400 ] || fail "the two reloads were ${gap} ms apart, expected >= 400"
stop_mock
note "reloads are spaced (>= gap, measured by the mock's timestamps)"

# 6. an unreachable port fails: port 1 is privileged, so nothing listens there.
if node "$HELPER" --port 1 --once >"$SCRATCH/o6" 2>&1; then
  fail "an unreachable port should exit non-zero"
else
  rc=$?
  [ "$rc" = "1" ] || fail "an unreachable port should exit 1, got $rc"
fi
grep -q "unreachable" "$SCRATCH/o6" || fail "the failure should name the unreachable port"
note "an unreachable port fails with exit 1"

# 7. a port with no chat-stasher worker is not an error: the tabs are still
#    ensured and the pass says the leg's state is unknown.
start_mock no-http-port 0 "$ALL_ALLOWED,example.com"
port="$(mock_port)"
if ! node "$HELPER" --port "$port" --once >"$SCRATCH/o7" 2>&1; then
  cat "$SCRATCH/o7" >&2
  fail "a missing worker should not fail the pass"
fi
grep -q "no chat-stasher service worker" "$SCRATCH/o7" || fail "the missing worker should be reported"
grep -q "the tabs are still ensured" "$SCRATCH/o7" || fail "the pass should say the tabs are still ensured"
[ -s "$SCRATCH/records.jsonl" ] && [ "$(wc -l < "$SCRATCH/records.jsonl" | tr -d ' ')" != "0" ] && fail "with the leg unknown there is nothing to reload"
stop_mock
note "a missing worker is reported and the tabs are still ensured"

# 8. a leg that has never ticked (no record) is not a no-http-port: no reloads.
start_mock none 1 "$ALL_ALLOWED,example.com"
port="$(mock_port)"
if ! node "$HELPER" --port "$port" --once >"$SCRATCH/o8" 2>&1; then
  cat "$SCRATCH/o8" >&2
  fail "a no-record pass should exit 0"
fi
grep -q "leg reports no-record" "$SCRATCH/o8" || fail "the no-record state should be reported"
[ -s "$SCRATCH/records.jsonl" ] && [ "$(wc -l < "$SCRATCH/records.jsonl" | tr -d ' ')" != "0" ] && fail "a leg that never ticked must not be reloaded"
stop_mock
note "a leg that has never ticked is distinct from no-http-port and is not reloaded"

# 9. --port is validated up front as a usage error.
if node "$HELPER" --port not-a-port --once >"$SCRATCH/o9" 2>&1; then
  fail "a bad --port should fail"
else
  rc=$?
  [ "$rc" = "2" ] || fail "a bad --port should exit 2 (usage), got $rc"
fi
grep -q -- "--port must be a TCP port number" "$SCRATCH/o9" || fail "the usage error should name --port"
note "--port is validated up front as a usage error"

# 10. asking for help is not an error: the usage line goes to stdout and the
#     exit is 0, so `helper --help` in a script does not read as a failure.
if ! node "$HELPER" --help >"$SCRATCH/o10" 2>&1; then
  fail "--help should exit 0"
fi
grep -q "^usage: cdp-keep-platform-tabs.mjs --port" "$SCRATCH/o10" || fail "--help should print the usage line"
note "--help prints the usage line and exits 0"

# 11. a tab on the host a platform redirects through is that platform's tab:
#     it is reloaded like any allowlisted one, and the canonical host is not
#     opened beside it. This is the other half of "one tab per platform".
start_mock no-http-port 1 "chat.openai.com,grok.com"
port="$(mock_port)"
if ! CS_TAB_RELOAD_GAP_MS=50 node "$HELPER" --port "$port" --once >"$SCRATCH/o11" 2>&1; then
  cat "$SCRATCH/o11" >&2
  fail "a pass over a redirect-spelling tab should exit 0"
fi
[ "$(reload_count chat.openai.com)" = "1" ] || fail "the redirect-spelling tab is allowlisted and should be reloaded once"
[ "$(open_count chatgpt.com)" = "0" ] || fail "chatgpt already has a tab, so its canonical host must not be opened"
grep -q "platforms with a tab: 2/7" "$SCRATCH/o11" || fail "two platforms should count as covered: $(cat "$SCRATCH/o11")"
stop_mock
note "a redirect-spelling tab counts as that platform's tab and is not doubled"

# The run must not leave a mock behind (see the same check in
# test-reload-extension.sh for why this is asserted rather than assumed).
leftover=""
if command -v pgrep >/dev/null 2>&1; then
  leftover="$(pgrep -f "$SCRATCH/cdp-mock.mjs" 2>/dev/null || true)"
else
  echo "note: pgrep not found; the leftover check examined only this run's recorded pids"
  while read -r pid; do
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then leftover="$leftover $pid"; fi
  done < "$MOCK_PIDS_FILE"
fi
if [ -n "${leftover// /}" ]; then
  echo "FAIL: this run left cdp-mock processes running (pids: $(printf '%s' "$leftover" | tr '\n' ' '))" >&2
  exit 1
fi

echo
echo "test-cdp-keep-platform-tabs: ${PASS} cases passed"
