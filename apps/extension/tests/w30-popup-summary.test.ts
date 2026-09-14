/**
 * W30 · The popup's one summary line and its "Open dashboard" button.
 *
 * ## What this file is protecting
 *
 * A summary line is exactly the kind of surface where "unknown" quietly becomes
 * "0": the host could not count, the page shows `0 sessions`, and the user
 * concludes their archive is empty. Every count on this line therefore carries
 * its own state — a measured number, or the word *unknown* with the reason in
 * the notes — and `complete` has to agree with the parts.
 *
 * The button has its own two rules, and both are asserted below:
 *   1. nothing is opened unless the host answered `ok` with a loopback URL
 *      (§10: the URL in a successful `open_dashboard` response is the *only*
 *      thing the popup may open);
 *   2. a button that cannot do anything is disabled, and says why.
 *
 * Two halves: the wire (a synthetic host, so the shapes the extension accepts
 * and refuses are pinned against the contract) and the render layer (pure, so
 * the sentences are asserted without a browser).
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { createSyntheticHost, SYNTHETIC_DASHBOARD_URL } from './synthetic-native-host';
import {
  openDashboard,
  parseDashboardUrl,
  summary as askSummary,
  type StageSummary,
} from '../lib/native-host';
import {
  dashboardButton,
  harnessBreakdown,
  openDashboardTab,
  popupText,
  renderPopup,
  summaryLine,
  NO_FAILURES,
  type PopupModel,
  type SummaryState,
} from '../lib/popup-view';

const NOW = Date.parse('2026-09-14T12:00:00.000Z');
const NOW_UNIX = Math.round(NOW / 1000);

const known = (count: number) => ({ kind: 'known' as const, count });

/**
 * A complete answer: 12 sessions in the window, 41 in the stage, pushed 2 h ago.
 * The buckets add up to both totals on purpose — the extension's own validator
 * refuses a split that does not, so a fixture that "looked right" without that
 * property would be testing a response the host is not allowed to send.
 */
const COMPLETE: StageSummary = {
  windowHours: 24,
  complete: true,
  total: known(41),
  last24h: known(12),
  byHarness: [
    { harness: 'deepseek', total: known(30), last_24h: known(9) },
    { harness: 'chatgpt', total: known(11), last_24h: known(3) },
  ],
  lastPush: { kind: 'known', unix: NOW_UNIX - 2 * 3600 },
};

/** Each part unreadable in its own way: the line must say unknown three times. */
const PARTIAL: StageSummary = {
  windowHours: 24,
  complete: false,
  total: {
    kind: 'unknown',
    why: '1 part of the stage could not be read, so the session count is a lower bound',
  },
  last24h: { kind: 'unknown', why: 'a shard had no readable mtime' },
  byHarness: [],
  lastPush: { kind: 'unknown', why: 'no run-once pass has ever been recorded' },
};

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    now: NOW,
    ...overrides,
  };
}

const answered = (summary: StageSummary): PopupModel => model({ summary: { kind: 'answer', summary } });

// --------------------------------------------------------------- the wire

interface Installed {
  host: ReturnType<typeof createSyntheticHost>;
}

function install(host: ReturnType<typeof createSyntheticHost>): Installed {
  const fake: any = {
    runtime: {
      id: 'w30-popup-summary-test',
      lastError: undefined,
      sendNativeMessage: (name: string, message: unknown) => host.sendNativeMessage(name, message),
    },
  };
  const browser = withI18n(fake);
  vi.stubGlobal('browser', browser);
  vi.stubGlobal('chrome', browser);
  return { host };
}

beforeEach(() => {
  vi.unstubAllGlobals();
});

describe('W30 · the wire: §6.4 summary and §6.5 open_dashboard', () => {
  it('asks for a summary with no parameters, and reads what the host counted', async () => {
    const { host } = install(createSyntheticHost({ summary: wireSummary(COMPLETE) }));
    const result = await askSummary();

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.summary.complete).toBe(true);
    expect(result.summary.windowHours).toBe(24);
    expect(result.summary.total).toEqual({ kind: 'known', count: 41 });
    expect(result.summary.last24h).toEqual({ kind: 'known', count: 12 });
    expect(result.summary.byHarness).toEqual(COMPLETE.byHarness);
    expect(result.summary.lastPush).toEqual({ kind: 'known', unix: NOW_UNIX - 2 * 3600 });

    // §6.4: parameterless. An extra field is refused by the host, so the
    // extension must never send one.
    expect(host.requests()).toEqual([{ protocol: 1, type: 'summary' }]);
  });

  it('carries an unknown count through as unknown, with the host reason', async () => {
    const { host } = install(createSyntheticHost({ summary: wireSummary(PARTIAL) }));
    const result = await askSummary();

    expect(host.summaryCount()).toBe(1);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.summary.complete).toBe(false);
    expect(result.summary.total.kind).toBe('unknown');
    if (result.summary.total.kind !== 'unknown') return;
    expect(result.summary.total.why).toContain('lower bound');
  });

  it('refuses a summary carrying a field the contract does not define', async () => {
    const broken = { ...wireSummary(COMPLETE), machine: 'somewhere' } as Record<string, any>;
    install(createSyntheticHost({ summary: broken }));
    const result = await askSummary();
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toBe('malformed-response');
    expect(result.detail).toContain('machine');
  });

  it('refuses a summary whose per-harness split does not add up to its total', async () => {
    const broken = wireSummary(COMPLETE) as Record<string, any>;
    broken.sessions.by_harness[0].total = { kind: 'known', count: 100 };
    install(createSyntheticHost({ summary: broken }));
    const result = await askSummary();
    expect(result.ok).toBe(false);
    if (result.ok) return;
    // The two numbers are both in the message: what the split says, and what
    // the total says. A reader can tell which one to distrust.
    expect(result.detail).toContain('sum to');
    expect(result.detail).toContain('not 41');
  });

  it('refuses a summary that claims to be complete while a part is unknown', async () => {
    const broken = wireSummary(PARTIAL) as Record<string, any>;
    broken.complete = true;
    install(createSyntheticHost({ summary: broken }));
    const result = await askSummary();
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.detail).toContain('complete');
  });

  it('names the host as older when it refuses the message it does not know', async () => {
    install(createSyntheticHost({ unsupported: true }));
    const result = await askSummary();
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toBe('nack');
    expect(result.olderHost).toBe(true);
    // 🔴 The conclusion is carried *and* the host's own words, because
    //    "overwhelmingly likely" is not "certain".
    expect(result.detail).toContain('unknown message type');
  });

  it('reports a missing host as a failure and never as an empty stage', async () => {
    install(createSyntheticHost({ up: false }));
    const result = await askSummary();
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.reason).toBe('send-failed');
    expect(result.olderHost).toBe(false);
  });

  it('returns the dashboard URL from a successful answer, and nothing on a refusal', async () => {
    install(createSyntheticHost());
    const opened = await openDashboard();
    expect(opened).toEqual({ ok: true, url: SYNTHETIC_DASHBOARD_URL });

    install(
      createSyntheticHost({
        dashboardNack: { kind: 'config', retryable: false, detail: 'no [native_host] destination' },
      }),
    );
    const refused = await openDashboard();
    expect(refused.ok).toBe(false);
    if (refused.ok) return;
    expect(refused.kind).toBe('config');
    expect(refused.detail).toContain('[native_host] destination');
  });

  it('refuses a dashboard URL that is not the loopback shape it asked for', async () => {
    for (const url of [
      'http://localhost:51234/?token=' + 'ab'.repeat(32),
      'http://127.0.0.1:51234/',
      'https://127.0.0.1:51234/?token=' + 'ab'.repeat(32),
      'http://127.0.0.1:51234/?token=' + 'ab'.repeat(31),
      'http://127.0.0.1:51234/?token=' + 'AB'.repeat(32),
    ]) {
      install(createSyntheticHost({ dashboardUrl: url }));
      const result = await openDashboard();
      expect(result.ok, `accepted ${url}`).toBe(false);
    }
  });

  it('accepts exactly the URL the host prints', () => {
    expect(parseDashboardUrl(SYNTHETIC_DASHBOARD_URL)).toBe(SYNTHETIC_DASHBOARD_URL);
    expect(parseDashboardUrl(`${SYNTHETIC_DASHBOARD_URL}\n`)).toBeNull();
    expect(parseDashboardUrl(undefined)).toBeNull();
  });
});

// ------------------------------------------------------------- the line

describe('W30 · the summary line states only what the host counted', () => {
  it('renders the counts, the split, the total and the push age', () => {
    expect(summaryLine(answered(COMPLETE))).toBe(
      'Last 24 h: 12 sessions (deepseek 9, chatgpt 3) · 41 in the stage · last push 2 h ago',
    );
  });

  it('says unknown for every part it could not read, and never zero', () => {
    const line = summaryLine(answered(PARTIAL));
    expect(line).toBe('Last 24 h: unknown · stage total unknown · last push unknown');
    expect(line).not.toContain('0');
  });

  it('puts each unknown part in the notes, with the host reason', () => {
    const view = renderPopup(answered(PARTIAL));
    const notes = view.notes.join('\n');
    expect(notes).toContain('Why the stage total is unknown');
    expect(notes).toContain('lower bound');
    expect(notes).toContain('Why the last push time is unknown');
    expect(notes).toContain('no run-once pass has ever been recorded');
  });

  it('does not add a note when nothing is unknown', () => {
    const view = renderPopup(answered(COMPLETE));
    expect(view.notes.join('\n')).not.toContain('unknown');
  });

  it('says the host is missing rather than showing an empty stage', () => {
    const line = summaryLine(
      model({ summary: { kind: 'failed', reason: 'send-failed', detail: 'not found', olderHost: false } }),
    );
    expect(line).toContain('no answer from the chat-stasher host');
    expect(line).not.toContain('0');
  });

  it('says the installed host is older than this extension', () => {
    const line = summaryLine(
      model({ summary: { kind: 'failed', reason: 'nack', detail: 'unknown message type', olderHost: true } }),
    );
    expect(line).toContain('older than this extension');
    expect(renderPopup(
      model({ summary: { kind: 'failed', reason: 'nack', detail: 'unknown message type', olderHost: true } }),
    ).notes.join('\n')).toContain('unknown message type');
  });

  it('labels a session with no harness prefix instead of dropping it', () => {
    const summary: StageSummary = {
      ...COMPLETE,
      byHarness: [{ harness: null, total: known(2), last_24h: known(2) }],
    };
    expect(harnessBreakdown(summary)).toBe('(no harness prefix) 2');
  });

  it('summarises the tail of a long split instead of truncating it silently', () => {
    const rows = ['a', 'b', 'c', 'd', 'e', 'f', 'g'].map((harness, index) => ({
      harness,
      total: known(10 - index),
      last_24h: known(10 - index),
    }));
    const breakdown = harnessBreakdown({ ...COMPLETE, byHarness: rows });
    expect(breakdown).toContain('a 10');
    expect(breakdown).toContain('and 2 more');
  });

  it('shows a push time in the future as a timestamp, not as a negative age', () => {
    const future: StageSummary = { ...COMPLETE, lastPush: { kind: 'known', unix: NOW_UNIX + 3 * 3600 } };
    const line = summaryLine(answered(future));
    expect(line).toContain('has not happened yet');
    expect(line).not.toContain('-3');
  });

  it('shows the window length the host reported, not a number of its own', () => {
    const line = summaryLine(answered({ ...COMPLETE, windowHours: 48 }));
    expect(line).toContain('Last 48 h:');
  });
});

// ------------------------------------------------------------ the button

describe('W30 · the dashboard button', () => {
  it('is enabled once the host has answered, whatever the counts say', () => {
    expect(dashboardButton(answered(COMPLETE))).toEqual({ label: 'Open dashboard', enabled: true, reason: null });
    // Unknown counts do not mean "the host is unreachable": the button still works.
    expect(dashboardButton(answered(PARTIAL)).enabled).toBe(true);
  });

  it('is disabled with a stated reason when there is no usable answer', () => {
    const unasked = dashboardButton(model({}));
    expect(unasked.enabled).toBe(false);
    expect(unasked.reason).toContain('has not answered a summary request');

    const missing = dashboardButton(
      model({ summary: { kind: 'failed', reason: 'send-failed', detail: 'not found', olderHost: false } }),
    );
    expect(missing.enabled).toBe(false);
    expect(missing.reason).toContain('does not answer');

    const older = dashboardButton(
      model({ summary: { kind: 'failed', reason: 'nack', detail: 'unknown message type', olderHost: true } }),
    );
    expect(older.enabled).toBe(false);
    expect(older.reason).toContain('older than this extension');
  });

  it('appears in the flattened popup text, with its reason', () => {
    const text = popupText(renderPopup(model({})));
    expect(text).toContain('[Button] Open dashboard');
    expect(text).toContain('has not answered a summary request');
  });
});

// ------------------------------------------------------- the button flow

describe('W30 · opening a tab: only ever the URL the host answered with', () => {
  it('opens exactly the host URL, once', async () => {
    const opened: string[] = [];
    const outcome = await openDashboardTab(
      async () => ({ ok: true, url: SYNTHETIC_DASHBOARD_URL }),
      (url) => {
        opened.push(url);
      },
    );
    expect(opened).toEqual([SYNTHETIC_DASHBOARD_URL]);
    expect(outcome.url).toBe(SYNTHETIC_DASHBOARD_URL);
    expect(outcome.message).toContain('opened in a new tab');
  });

  it('opens nothing when the host refuses, and says what it refused with', async () => {
    const opened: string[] = [];
    const outcome = await openDashboardTab(
      async () => ({ ok: false, reason: 'nack', kind: 'config', detail: 'no [native_host] destination', retryable: false, olderHost: false }),
      (url) => {
        opened.push(url);
      },
    );
    expect(opened).toEqual([]);
    expect(outcome.url).toBeNull();
    expect(outcome.message).toContain('no [native_host] destination');
  });

  it('opens nothing when the host is not there at all', async () => {
    const opened: string[] = [];
    const outcome = await openDashboardTab(
      async () => {
        throw new Error('Specified native messaging host not found.');
      },
      (url) => {
        opened.push(url);
      },
    );
    expect(opened).toEqual([]);
    expect(outcome.message).toContain('Specified native messaging host not found.');
  });

  it('opens nothing when the answer is a URL this extension will not open', async () => {
    const opened: string[] = [];
    const outcome = await openDashboardTab(
      // A response that says ok but points somewhere else. The wire validator
      // refuses this before it gets here; this pins the second, independent check.
      async () => ({ ok: true, url: 'http://evil.test:51234/?token=' + 'ab'.repeat(32) }),
      (url) => {
        opened.push(url);
      },
    );
    expect(opened).toEqual([]);
    expect(outcome.url).toBeNull();
    expect(outcome.message).toContain('did not open it');
  });

  it('reports a failure to open the tab instead of claiming it opened', async () => {
    const outcome = await openDashboardTab(
      async () => ({ ok: true, url: SYNTHETIC_DASHBOARD_URL }),
      () => {
        throw new Error('tabs.create is unavailable');
      },
    );
    expect(outcome.url).toBeNull();
    expect(outcome.message).toContain('tabs.create is unavailable');
  });
});

/** A `StageSummary` in the host's wire shape (§6.4), for the synthetic host. */
function wireSummary(summary: StageSummary): Record<string, any> {
  return {
    protocol: 1,
    type: 'summary',
    ok: true,
    window_hours: summary.windowHours,
    complete: summary.complete,
    sessions: {
      total: wireCount(summary.total),
      last_24h: wireCount(summary.last24h),
      by_harness: summary.byHarness.map((row) => ({
        harness: row.harness,
        total: wireCount(row.total),
        last_24h: wireCount(row.last_24h),
      })),
    },
    last_push:
      summary.lastPush.kind === 'known'
        ? { kind: 'known', unix: summary.lastPush.unix }
        : { kind: 'unknown', why: summary.lastPush.why },
  };
}

function wireCount(state: StageSummary['total']): Record<string, unknown> {
  return state.kind === 'known' ? { kind: 'known', count: state.count } : { kind: 'unknown', why: state.why };
}
