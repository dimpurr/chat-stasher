#!/usr/bin/env node
// cdp-reload-extension.mjs — reload a running chat-stasher extension over the
// Chrome DevTools Protocol and check that the worker comes back on the version
// that was just built.
//
// Why it exists: reload-extension.sh swaps the unpacked build on disk, but
// Chrome keeps running the old service worker until something reloads the
// extension. Restarting the browser is not enough — it re-reads the manifest
// but can leave the old worker in place, which is the "new manifest + old
// worker" state that is hardest to notice. Calling chrome.runtime.reload() from
// the worker is the action that actually restarts it, and this helper is that
// call plus the assertion that it worked.
//
// Usage:
//   node cdp-reload-extension.mjs --port <port> [--expected-version <v>]
//
//   --port <port>            Chrome's --remote-debugging-port (required).
//   --expected-version <v>   Version the reloaded worker must report. When it
//                            is omitted the helper only reloads and prints the
//                            version it saw.
//
// Exit codes: 0 = reloaded (and, when asked, verified) · 1 = no matching
// worker, the reload failed, or the worker never came back on the expected
// version · 2 = usage error.
//
// The user-visible name is what identifies the extension. The manifest name is
// the __MSG_extName__ placeholder and chrome.i18n resolves it, so both are
// checked against /chat.?stasher/i — the same match the reference helper used.
//
// Test-only: CS_CDP_TIMEOUT_MS overrides the 15 s budget for finding the worker
// and for waiting for it to come back, so a test can assert the timeout path
// without waiting it out.

const DEFAULT_TIMEOUT_MS = 15000;
const POLL_MS = 100;
const EVAL_TIMEOUT_MS = 5000;
const NAME_RE = /chat.?stasher/i;

const MANIFEST_EXPR =
  'JSON.stringify({name: chrome.runtime.getManifest().name, ' +
  'dn: (chrome.i18n && chrome.i18n.getMessage) ? chrome.i18n.getMessage("extName") : "", ' +
  'version: chrome.runtime.getManifest().version})';
// The reload is deferred by a tick so the evaluate reply can travel back over
// the socket before the worker tears itself down. The reply only proves the
// expression ran; the version read below is what proves the restart.
const RELOAD_EXPR = 'setTimeout(() => chrome.runtime.reload(), 50), "reload-sent"';

function usage(message) {
  if (message) console.error(`cdp-reload-extension: ${message}`);
  console.error('usage: cdp-reload-extension.mjs --port <port> [--expected-version <v>]');
  process.exit(2);
}

function parseArgs(argv) {
  const args = { port: null, expected: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--port') {
      if (i + 1 >= argv.length) usage('--port needs a value');
      args.port = argv[i + 1];
      i += 1;
    } else if (arg === '--expected-version') {
      if (i + 1 >= argv.length) usage('--expected-version needs a value');
      args.expected = argv[i + 1];
      i += 1;
    } else if (arg === '-h' || arg === '--help') {
      usage();
    } else {
      usage(`unknown argument: ${arg}`);
    }
  }
  if (!args.port) usage('--port is required');
  if (!/^[0-9]{1,5}$/.test(args.port) || Number(args.port) < 1 || Number(args.port) > 65535) {
    usage(`--port must be a TCP port number, got: ${args.port}`);
  }
  return args;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function listTargets(port) {
  const res = await fetch(`http://127.0.0.1:${port}/json/list`, {
    signal: AbortSignal.timeout(3000),
  });
  if (!res.ok) throw new Error(`HTTP ${res.status} from /json/list`);
  const body = await res.json();
  return Array.isArray(body) ? body : [];
}

function isExtensionWorker(target) {
  return (
    target &&
    target.type === 'service_worker' &&
    typeof target.url === 'string' &&
    target.url.startsWith('chrome-extension://') &&
    typeof target.webSocketDebuggerUrl === 'string'
  );
}

// One Runtime.evaluate round trip. Reacts only to the reply id this call used,
// and always settles exactly once: a dead socket, a malformed frame, and the
// timeout all resolve to null rather than hanging the caller.
function evaluate(target, expression) {
  return new Promise((resolve) => {
    let settled = false;
    let socket = null;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (socket) {
        try { socket.close(); } catch { /* best-effort close */ }
      }
      resolve(value);
    };
    const timer = setTimeout(() => finish(null), EVAL_TIMEOUT_MS);
    try {
      socket = new WebSocket(target.webSocketDebuggerUrl);
    } catch {
      finish(null);
      return;
    }
    socket.onopen = () => socket.send(JSON.stringify({
      id: 1,
      method: 'Runtime.evaluate',
      params: { expression, returnByValue: true },
    }));
    socket.onmessage = (event) => {
      let message;
      try { message = JSON.parse(event.data); } catch { return; }
      if (message.id !== 1) return;
      const value = message.result && message.result.result ? message.result.result.value : undefined;
      finish(typeof value === 'string' ? value : null);
    };
    socket.onerror = () => finish(null);
  });
}

async function readManifest(target) {
  const raw = await evaluate(target, MANIFEST_EXPR);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

function isOurs(manifest) {
  return Boolean(manifest) && NAME_RE.test(`${manifest.dn || ''} ${manifest.name || ''}`);
}

function fail(message) {
  console.error(`cdp-reload-extension: ${message}`);
  process.exit(1);
}

async function main() {
  if (typeof WebSocket === 'undefined') {
    fail('this node has no global WebSocket; Node 22 or newer is required');
  }
  const args = parseArgs(process.argv.slice(2));
  const budget = Number(process.env.CS_CDP_TIMEOUT_MS || DEFAULT_TIMEOUT_MS);
  if (!Number.isFinite(budget) || budget <= 0) {
    usage('CS_CDP_TIMEOUT_MS must be a positive number');
  }

  // Phase 1: find our worker. A port that never answers and a port that
  // answers with no matching worker are reported differently, because they mean
  // different things: the first is the wrong --cdp-port, the second can be a
  // worker that is still starting.
  const findDeadline = Date.now() + budget;
  let target = null;
  let lastError = null;
  let reachedPort = false;
  let transportErrors = 0;
  while (Date.now() < findDeadline && !target) {
    try {
      for (const candidate of (await listTargets(args.port)).filter(isExtensionWorker)) {
        if (isOurs(await readManifest(candidate))) {
          target = candidate;
          break;
        }
      }
      reachedPort = true;
      transportErrors = 0;
    } catch (err) {
      lastError = err;
      transportErrors += 1;
      // A closed port does not open by itself, so three quick failures with no
      // successful list in between mean the port is wrong, not slow; fail now
      // instead of waiting the budget out.
      if (!reachedPort && transportErrors >= 3) break;
    }
    if (!target) await sleep(POLL_MS);
  }
  if (!target) {
    const detail = !reachedPort
      ? (lastError ? `last error: ${lastError.message}` : `no response from 127.0.0.1:${args.port}`)
      : 'the port answered but no chat-stasher service worker was listed';
    fail(`no chat-stasher service worker found on 127.0.0.1:${args.port} (${detail})`);
  }

  // Phase 2: reload.
  const ack = await evaluate(target, RELOAD_EXPR);
  if (ack !== 'reload-sent') {
    fail(`could not call chrome.runtime.reload() on the worker (${ack === null ? 'no reply' : ack})`);
  }
  console.log('cdp-reload-extension: chrome.runtime.reload() sent');

  if (!args.expected) {
    const manifest = await readManifest(target);
    if (manifest) console.log(`cdp-reload-extension: worker reports version ${manifest.version}`);
    return;
  }

  // Phase 3: the worker must come back on the expected version before the
  // budget runs out. This is the part the reload call alone cannot show.
  const verifyDeadline = Date.now() + budget;
  let lastSeen = null;
  while (Date.now() < verifyDeadline) {
    try {
      for (const candidate of (await listTargets(args.port)).filter(isExtensionWorker)) {
        const manifest = await readManifest(candidate);
        if (!isOurs(manifest)) continue;
        lastSeen = manifest.version;
        if (manifest.version === args.expected) {
          console.log(`cdp-reload-extension: reloaded and verified version ${args.expected}`);
          return;
        }
      }
    } catch {
      // The worker is mid-restart; keep polling until the budget runs out.
    }
    await sleep(POLL_MS);
  }
  fail(`the worker did not come back on ${args.expected} (last version seen: ${lastSeen === null ? 'none' : lastSeen})`);
}

main().catch((err) => fail(err && err.message ? err.message : String(err)));
