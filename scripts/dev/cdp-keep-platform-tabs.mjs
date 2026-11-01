#!/usr/bin/env node
// cdp-keep-platform-tabs.mjs — keep the platform tabs the backfill leg needs
// open in a dedicated CDP test browser, and reload them when the leg reports
// no-http-port.
//
// Why it exists: the backfill leg fetches through an open, logged-in page of
// each platform — the extension requests no host permissions at all — so with
// no page of a platform open there is no fetch channel and the tick stops at
// 'no-http-port' (apps/extension/lib/backfill/schedule.ts:237,
// apps/extension/entrypoints/background.ts:2600). Opening the tabs is not
// enough on its own: when Chrome reclaims the service worker, the injected
// channels go with it and the leg falls back to 'no-http-port' even with the
// tabs still open — only a reload of the page re-injects. This helper closes
// that loop. It is the repo's copy of the discipline the TBR run applied by
// hand with a throwaway script of its own, itself modelled on the W102 run's:
// an exact-host allowlist, one reload each, >=10 s apart, and nothing outside
// the allowlist is ever touched.
//
// It is opt-in and dev-only. It drives a real browser over CDP, so it must
// never be pointed at a browser whose tabs you care about: point it at the
// dedicated test browser and pass that browser's --remote-debugging-port. It
// reads no conversation data and prints none — hosts, counts and the leg's
// named outcome only.
//
// What one pass does:
//   1. Read the leg's last tick (cs_backfill_lasttick_v1, the key
//      lib/backfill/alarm.ts:1287 stores) from the extension's own storage,
//      over the service worker. No chat-stasher worker listed means the
//      extension is not loaded in this browser, or the worker is mid-restart:
//      the pass says so and still ensures the tabs.
//   2. Make sure each platform has a tab, opening its canonical host for any
//      that has none. A tab already open on a spelling that platform redirects
//      through counts, so no platform ever gets two tabs. A tab opened by this
//      pass is fresh and is not reloaded by it.
//   3. When the leg reports no-http-port, reload every open allowlisted tab —
//      one reload each, spaced — so the next tick has a channel again.
//
// Usage:
//   node cdp-keep-platform-tabs.mjs --port <port> [--interval <ms>] [--once] [--dry-run]
//
//   --port <port>      Chrome's --remote-debugging-port (required).
//   --interval <ms>    Pass interval (default 300000 = 5 min). Ignored by --once.
//   --once             Run a single pass and exit.
//   --dry-run          Print what a pass would do; open and reload nothing.
//
// Exit codes: 0 = the pass completed · 1 = something failed — in --once mode
// directly; in loop mode for a pass that could not complete (the next
// interval retries it, so one bad tab does not stop the keepalive), or when
// the port is unreachable or the leg's state could not be read (there the
// loop ends: a closed debugging port does not reopen and a worker that lists
// but does not answer is not retried into health) · 2 = usage error.
//
// Test-only: CS_TAB_RELOAD_GAP_MS overrides the 10 s spacing between reloads,
// so a test can assert the spacing without waiting it out.

const DEFAULT_INTERVAL_MS = 300000;
const DEFAULT_GAP_MS = 10000;
const LIST_TIMEOUT_MS = 3000;
const EVAL_TIMEOUT_MS = 5000;
const RELOAD_TIMEOUT_MS = 15000;
const NAME_RE = /chat.?stasher/i;
const LAST_TICK_KEY = 'cs_backfill_lasttick_v1';

// One entry per platform the leg can fetch for (apps/extension/lib/contract.ts:359-783).
// The first host of each group is the one this helper opens; the rest are the
// spellings that platform's own pages redirect through, so an open tab on any
// of them satisfies the platform — opening both would put two tabs of the same
// platform in the window and reload each of them every pass. Hosts are matched
// exactly, never as suffixes: a page is reloaded only when its host is one of
// these.
const PLATFORM_TABS = [
  ['chatgpt.com', 'chat.openai.com'],
  ['chat.deepseek.com'],
  ['gemini.google.com'],
  ['grok.com'],
  ['claude.ai'],
  ['www.perplexity.ai', 'perplexity.ai'],
  ['www.kimi.com', 'kimi.com'],
];
const ALLOWED_HOSTS = new Set(PLATFORM_TABS.flat());

const MANIFEST_EXPR =
  'JSON.stringify({name: chrome.runtime.getManifest().name, ' +
  'dn: (chrome.i18n && chrome.i18n.getMessage) ? chrome.i18n.getMessage("extName") : "", ' +
  'version: chrome.runtime.getManifest().version})';
// The storage read is a promise, so it is evaluated with awaitPromise; the
// expression resolves to a JSON string either way, and '{}' when the leg has
// never ticked — "no record" stays distinct from "could not read".
const STORAGE_EXPR =
  `new Promise((resolve) => chrome.storage.local.get(${JSON.stringify(LAST_TICK_KEY)}, ` +
  '(v) => resolve(JSON.stringify(v))))';

// A message is a usage *error* and goes to stderr with exit 2; without one this
// is `--help`, which goes to stdout and exits 0 — a script that runs this helper
// must be able to ask it how without reading that as a failure.
function usage(message) {
  const out = message ? console.error : console.log;
  if (message) out(`cdp-keep-platform-tabs: ${message}`);
  out('usage: cdp-keep-platform-tabs.mjs --port <port> [--interval <ms>] [--once] [--dry-run]');
  process.exit(message ? 2 : 0);
}

function parseArgs(argv) {
  const args = { port: null, interval: DEFAULT_INTERVAL_MS, once: false, dryRun: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--port') {
      if (i + 1 >= argv.length) usage('--port needs a value');
      args.port = argv[i + 1];
      i += 1;
    } else if (arg === '--interval') {
      if (i + 1 >= argv.length) usage('--interval needs a value');
      args.interval = Number(argv[i + 1]);
      i += 1;
    } else if (arg === '--once') {
      args.once = true;
    } else if (arg === '--dry-run') {
      args.dryRun = true;
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
  if (!Number.isFinite(args.interval) || args.interval <= 0) {
    usage('--interval must be a positive number of milliseconds');
  }
  return args;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function listTargets(port) {
  const res = await fetch(`http://127.0.0.1:${port}/json/list`, {
    signal: AbortSignal.timeout(LIST_TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(`HTTP ${res.status} from /json/list`);
  const body = await res.json();
  return Array.isArray(body) ? body : [];
}

// One Runtime.evaluate round trip. Reacts only to the reply id this call used,
// and always settles exactly once: a dead socket, a malformed frame and the
// timeout all resolve to null rather than hanging the caller.
function evaluate(target, expression, awaitPromise) {
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
      params: { expression, returnByValue: true, awaitPromise },
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
  const raw = await evaluate(target, MANIFEST_EXPR, false);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
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

function isOurs(manifest) {
  return Boolean(manifest) && NAME_RE.test(`${manifest.dn || ''} ${manifest.name || ''}`);
}

// One Page.reload round trip against a page target, same settle-once
// discipline as the evaluate above.
function reloadTarget(wsUrl) {
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
    const timer = setTimeout(() => finish({ ok: false, err: 'timeout' }), RELOAD_TIMEOUT_MS);
    try {
      socket = new WebSocket(wsUrl);
    } catch {
      finish({ ok: false, err: 'ws' });
      return;
    }
    socket.onopen = () => socket.send(JSON.stringify({ id: 1, method: 'Page.reload', params: { ignoreCache: false } }));
    socket.onmessage = (event) => {
      let message;
      try { message = JSON.parse(event.data); } catch { return; }
      if (message.id !== 1) return;
      finish({ ok: !message.error, err: message.error ? JSON.stringify(message.error) : null });
    };
    socket.onerror = () => finish({ ok: false, err: 'ws' });
  });
}

async function openTab(port, host) {
  const res = await fetch(`http://127.0.0.1:${port}/json/new?https://${host}/`, {
    method: 'PUT',
    signal: AbortSignal.timeout(LIST_TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(`HTTP ${res.status} from /json/new`);
  return res.json();
}

function hostOf(target) {
  try {
    return new URL(target.url).host;
  } catch {
    return '';
  }
}

function fail(message) {
  console.error(`cdp-keep-platform-tabs: ${message}`);
  process.exit(1);
}

// One pass. Returns true when the pass completed. Throws only when the port is
// unreachable — the one failure a keepalive loop cannot retry its way out of.
async function runPass(port, gapMs, dryRun) {
  let targets;
  try {
    targets = await listTargets(port);
  } catch (err) {
    throw new Error(`127.0.0.1:${port} is unreachable (${err.message}) — is this the test browser's debugging port?`);
  }

  // Phase 1: the leg's state, read from the extension's own storage over the
  // service worker. A port that answers with no chat-stasher worker is not an
  // error: the worker may be mid-restart, which is one of the two things this
  // helper exists for, and the tabs are ensured regardless.
  let leg = 'unknown';
  let workerFound = false;
  for (const candidate of targets.filter(isExtensionWorker)) {
    const manifest = await readManifest(candidate);
    if (!isOurs(manifest)) continue;
    workerFound = true;
    const raw = await evaluate(candidate, STORAGE_EXPR, true);
    if (raw === null) {
      fail(`could not read ${LAST_TICK_KEY} from the worker`);
    }
    let record = null;
    try {
      const parsed = JSON.parse(raw);
      record = parsed && typeof parsed === 'object' ? parsed[LAST_TICK_KEY] : null;
    } catch {
      record = null;
    }
    if (!record || typeof record !== 'object') {
      leg = 'no-record';
    } else {
      // Two fields, deliberately: `reason` is the tick's named outcome, and
      // `stopped` carries the same value for a tick that was blocked at a gate
      // before any run happened (alarm.ts:1708-1718) — which is the case here,
      // a tick that never reached a page. Reading only `reason` would miss it.
      leg = (record.reason === 'no-http-port' || record.stopped === 'no-http-port')
        ? 'no-http-port'
        : 'ok';
    }
    break;
  }
  if (workerFound) {
    console.log(`cdp-keep-platform-tabs: leg reports ${leg}`);
  } else {
    console.log(`cdp-keep-platform-tabs: no chat-stasher service worker on 127.0.0.1:${port} — the leg's state is unknown this pass; the tabs are still ensured`);
  }

  // Phase 2: the tabs. Hosts are matched exactly; anything outside the
  // allowlist is counted and left untouched. A platform counts as having a tab
  // when any of its spellings is open, so the redirect forms never double up.
  const pages = targets.filter((t) => t.type === 'page');
  const allowedPages = pages.filter((p) => ALLOWED_HOSTS.has(hostOf(p)));
  const untouched = pages.length - allowedPages.length;
  const openHosts = new Set(allowedPages.map(hostOf));
  const hasTab = (hosts) => hosts.some((host) => openHosts.has(host));
  const satisfied = PLATFORM_TABS.filter(hasTab).length;
  const missing = PLATFORM_TABS.filter((hosts) => !hasTab(hosts)).map((hosts) => hosts[0]);

  let ok = true;
  if (missing.length > 0) {
    if (dryRun) {
      console.log(`cdp-keep-platform-tabs: dry run — would open ${missing.length} tab(s): ${missing.join(', ')}`);
    } else {
      for (const host of missing) {
        try {
          await openTab(port, host);
          console.log(`cdp-keep-platform-tabs: opened https://${host}/`);
        } catch (err) {
          ok = false;
          console.error(`cdp-keep-platform-tabs: could not open https://${host}/ (${err.message})`);
        }
      }
    }
  }

  // Phase 3: the reload, only when the leg says it has no channel. Tabs this
  // pass opened are fresh and are left alone; every already-open allowlisted
  // tab gets exactly one reload, spaced.
  if (leg === 'no-http-port' && allowedPages.length > 0) {
    if (dryRun) {
      console.log(`cdp-keep-platform-tabs: dry run — would reload ${allowedPages.length} tab(s), one each, ${gapMs} ms apart`);
    } else {
      for (const page of allowedPages) {
        const host = hostOf(page);
        const result = await reloadTarget(page.webSocketDebuggerUrl);
        if (!result.ok) {
          ok = false;
          console.error(`cdp-keep-platform-tabs: reloading ${host} failed (${result.err})`);
        } else {
          console.log(`cdp-keep-platform-tabs: reloaded ${host}`);
        }
        await sleep(gapMs);
      }
    }
  }

  console.log(`cdp-keep-platform-tabs: platforms with a tab: ${satisfied}/${PLATFORM_TABS.length}, allowed tabs open: ${allowedPages.length}, untouched page targets: ${untouched}`);
  return ok;
}

async function main() {
  if (typeof WebSocket === 'undefined') {
    fail('this node has no global WebSocket; Node 22 or newer is required');
  }
  const args = parseArgs(process.argv.slice(2));
  const gapMs = Number(process.env.CS_TAB_RELOAD_GAP_MS || DEFAULT_GAP_MS);
  if (!Number.isFinite(gapMs) || gapMs < 0) {
    usage('CS_TAB_RELOAD_GAP_MS must be a non-negative number');
  }

  let stopping = false;
  for (const sig of ['SIGINT', 'SIGTERM']) {
    process.on(sig, () => { stopping = true; });
  }

  while (!stopping) {
    let ok = true;
    try {
      ok = await runPass(args.port, gapMs, args.dryRun);
    } catch (err) {
      // Only the unreachable port throws. Everything a pass can fail on is
      // reported and carried in `ok` instead, so one bad tab does not stop
      // the keepalive.
      fail(err && err.message ? err.message : String(err));
    }
    if (args.once) process.exit(ok ? 0 : 1);
    if (stopping) break;
    if (!ok) console.log('cdp-keep-platform-tabs: the pass did not complete; the next one retries');
    await sleep(args.interval);
  }
}

main();
