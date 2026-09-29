/**
 * W248 · EXT-13 — an install id is a lineage, not a live browser profile.
 *
 * ADR-045 (accepted 2026-09-29) is the decision this file pins. A copied
 * browser profile carries `cs_install_identity_v1` verbatim, so the extension
 * cannot tell a copy from the original: identical `storage.local` means an
 * identical install id, an identical salt, an identical profile label, and no
 * window handle or profile directory a host could use to tell the two apart.
 * What a copy *cannot* carry is the other copy's future — each instance keeps
 * its own monotonic `report_seq`, and two live writers on one id eventually send
 * a repeated or regressing value. That divergence is the only positive evidence
 * of cloning the protocol has, and it is observed by the host, never here.
 *
 * This file covers the extension's half:
 *
 * ① `report_seq` is monotonic, lives in `storage.local`, and is *unknown* —
 *    never zero — when that storage cannot be read. A fabricated zero is the
 *    one value that reads as "this writer has never reported", which is exactly
 *    what a regression looks like, so guessing one would accuse an honest
 *    install of being a copy;
 * ② the repair mints a new install id and resets the sequence, and it is
 *    reachable *only* from the explicit user action: nothing on an automatic
 *    path rekeys a profile, because a restored backup or a renamed profile
 *    would then silently sever the wrong lineage;
 * ③ the popup shows the repair for a conflicted install and says plainly that
 *    the queued captures are still there and that old records keep the old id.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { getInstallIdentity, rekeyInstallIdentity } from '../lib/install-identity';
import { REPORT_SEQ_KEY, nextReportSeq, readReportSeq, resetReportSeq } from '../lib/report-seq';
import {
  NO_FAILURES,
  POPUP_REKEY_IDENTITY_MESSAGE,
  POPUP_REQUEST_IDENTITY_MESSAGE,
  popupText,
  renderPopup,
  type PopupModel,
} from '../lib/popup-view';
import { deliver, identityState, reportInstallStatus } from '../lib/native-host';
import { createSyntheticHost } from './synthetic-native-host';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const INSTALL = '11111111-1111-4111-8111-111111111111';
const AT = Date.parse('2026-09-29T10:00:00.000Z');

/**
 * `storage.local` with real read/write semantics, and a backing object so a
 * test can seed a corrupt value. Shaped like `lib/install-identity.ts` reads it,
 * including the `get(null)` snapshot the outbox drain uses.
 */
function fakeStorage(initial: Record<string, unknown> = {}) {
  const values: Record<string, unknown> = { ...initial };
  return {
    values,
    get: vi.fn(async (query: Record<string, unknown> | null) => {
      if (query === null) return { ...values };
      const key = Object.keys(query)[0]!;
      return { [key]: values[key] };
    }),
    set: vi.fn(async (items: Record<string, unknown>) => {
      Object.assign(values, items);
    }),
  };
}

/** `getRuntime` needs a runtime `id`; without one every send is refused. */
function stubRuntime(host: { sendNativeMessage: (host: string, message: unknown) => Promise<unknown> }): void {
  vi.stubGlobal('browser', { runtime: { id: 'w248-test', sendNativeMessage: host.sendNativeMessage } });
}

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    now: AT,
    ...overrides,
  };
}

beforeEach(() => {
  vi.resetModules();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('report_seq', () => {
  it('advances by one from a per-profile start and never goes backwards', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBe(1);
    expect(await nextReportSeq()).toBe(2);
    expect(await nextReportSeq()).toBe(3);
    expect(await readReportSeq()).toBe(3);
  });

  it('🔴 unreadable storage is unknown, never zero', async () => {
    vi.stubGlobal('chrome', { storage: { local: undefined } });
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBeNull();
  });

  it('🔴 a corrupt stored value is unknown, and is not silently overwritten', async () => {
    const storage = fakeStorage({ [REPORT_SEQ_KEY]: 'not a number' });
    vi.stubGlobal('chrome', { storage: { local: storage } });
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBeNull();
    expect(storage.set).not.toHaveBeenCalled();
  });

  it('reset clears it so a new identity starts its own sequence', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    await nextReportSeq();
    await nextReportSeq();
    await resetReportSeq();
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBe(1);
  });
});

describe('the repair mints a new identity', () => {
  it('mints a new install_id, keeps the labels, and resets the sequence', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const before = await getInstallIdentity();
    await nextReportSeq();
    await nextReportSeq();
    expect(await readReportSeq()).toBe(2);

    const after = await rekeyInstallIdentity();
    expect(after.install_id).toMatch(UUID);
    expect(after.install_id).not.toBe(before.install_id);
    expect(after.browser).toBe(before.browser);
    expect(after.profile_label).toBe(before.profile_label);
    expect(await readReportSeq()).toBeNull();
    expect(await getInstallIdentity()).toMatchObject({ install_id: after.install_id });
  });

  it('🔴 reading an identity twice never rekeys it', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const first = await getInstallIdentity();
    const second = await getInstallIdentity();
    expect(second.install_id).toBe(first.install_id);
  });

  it('🔴 a delivery that lands on a conflicted id does not rekey anything', async () => {
    // The property is an absence: no automatic caller exists. Asserting it by
    // name means adding one — a "helpful" rotation on a refused delivery, say —
    // has to delete this expectation to pass.
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const before = await getInstallIdentity();
    const host = createSyntheticHost({ up: true, identityConflict: true });
    stubRuntime(host);
    await deliver('deepseek-a.json', '{}', null);
    expect((await getInstallIdentity()).install_id).toBe(before.install_id);
  });
});

describe('the wire carries the sequence', () => {
  function statusBody(): Parameters<typeof reportInstallStatus>[0] {
    return {
      install_id: INSTALL,
      browser: 'Chrome',
      profile_label: 'Personal',
      extension_version: '0.4.0',
      reported_at: '2026-09-29T10:00:00Z',
      platforms: [],
    };
  }

  it('stamps a fresh sequence on every status report and every delivery', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const host = createSyntheticHost({ up: true });
    stubRuntime(host);

    await reportInstallStatus(statusBody());
    await deliver('deepseek-a.json', '{}', null);

    const status = host.requests().find((request) => request.type === 'status')!;
    const delivery = host.requests().find((request) => request.type === 'deliver')!;
    expect((status.status as Record<string, unknown>).report_seq).toBe(1);
    expect(delivery.report_seq).toBe(2);
  });

  it('🔴 omits report_seq rather than sending an unknown one as a number', async () => {
    vi.stubGlobal('chrome', { storage: { local: undefined } });
    const host = createSyntheticHost({ up: true });
    stubRuntime(host);

    await reportInstallStatus(statusBody());
    const status = host.requests().find((request) => request.type === 'status')!;
    expect(Object.hasOwn(status.status as object, 'report_seq')).toBe(false);
  });

  it('🔴 two overlapping sends leave in sequence order, so a slow one is not read as a copy', async () => {
    // The host compares arrival order and each native message is handled by its
    // own host process, so a delivery that is merely *slow* would otherwise be
    // indistinguishable from a second writer. The synthetic host is made to
    // resolve the first delivery last: without the shared sequence lock the
    // second message would be logged first, and its stamp would be lower.
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const host = createSyntheticHost({ up: true });
    let releaseFirst: (() => void) | null = null;
    let enteredFirst: (() => void) | null = null;
    const firstInFlight = new Promise<void>((resolve) => {
      enteredFirst = resolve;
    });
    let seen = 0;
    stubRuntime({
      async sendNativeMessage(name: string, message: unknown) {
        const first = seen === 0;
        seen += 1;
        if (first) {
          enteredFirst!();
          await new Promise<void>((resolve) => {
            releaseFirst = resolve;
          });
        }
        return host.sendNativeMessage(name, message);
      },
    });

    const firstDelivery = deliver('deepseek-a.json', '{"a":1}', null);
    const secondDelivery = deliver('deepseek-b.json', '{"b":2}', null);
    await firstInFlight;
    // The first message is in flight and held there. Flush everything the
    // second one could still be waiting on that is not the lock — its digest,
    // its storage read — so that "it has not been sent" is a statement about
    // the lock rather than about a microtask that has not run yet.
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(seen).toBe(1);
    expect(host.requests()).toHaveLength(0);
    releaseFirst!();
    await Promise.all([firstDelivery, secondDelivery]);

    // The purpose, not just the mechanism: the host compares arrival order, so
    // these two stamps have to arrive ascending. Sent concurrently, the held
    // one would land second and look exactly like a copy's regression.
    const seqs = host
      .requests()
      .filter((request) => request.type === 'deliver')
      .map((request) => request.report_seq as number);
    expect(seqs).toEqual([1, 2]);
  });
});

describe('identity_state', () => {
  it('answers unknown as a failure, not as "no conflict"', async () => {
    const host = createSyntheticHost({ up: false });
    stubRuntime(host);
    expect(await identityState(INSTALL)).toEqual({ ok: false });
  });

  it('refuses to ask about anything that is not a 36-character install id', async () => {
    const host = createSyntheticHost({ up: true, identityConflict: true });
    stubRuntime(host);
    expect(await identityState('not-an-id')).toEqual({ ok: false });
    expect(host.requests()).toHaveLength(0);
  });

  it('reads the host verdict', async () => {
    const host = createSyntheticHost({ up: true, identityConflict: true });
    stubRuntime(host);
    expect(await identityState(INSTALL)).toEqual({ ok: true, conflict: true });
    const calm = createSyntheticHost({ up: true, identityConflict: false });
    stubRuntime(calm);
    expect(await identityState(INSTALL)).toEqual({ ok: true, conflict: false });
  });
});

describe('the popup shows the repair', () => {
  it('renders it only while the host reports a conflict', () => {
    expect(renderPopup(model({ identityConflict: false })).identityRepair).toBeNull();
    expect(renderPopup(model()).identityRepair).toBeNull();
    const conflicted = renderPopup(model({ identityConflict: true }));
    expect(conflicted.identityRepair).not.toBeNull();
    expect(popupText(conflicted)).toContain(conflicted.identityRepair!.action.label);
  });

  it('🔴 says the queued captures are kept and the old records keep the old id', () => {
    const text = popupText(renderPopup(model({ identityConflict: true })));
    // The two facts a user acting on this has to be able to rely on: their
    // captures were not thrown away, and repairing does not rewrite history.
    expect(text).toMatch(/queue/i);
    expect(text).toMatch(/old identity/i);
  });

  it('carries the message types the popup sends', () => {
    expect(POPUP_REQUEST_IDENTITY_MESSAGE).toBe('cs-request-identity');
    expect(POPUP_REKEY_IDENTITY_MESSAGE).toBe('cs-rekey-identity');
  });
});
