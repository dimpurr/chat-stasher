/**
 * W248 · EXT-13 — an install id is a lineage, not a live browser profile.
 *
 * ADR-045 (accepted 2026-09-29) is the decision this file pins. A copied
 * browser profile carries `cs_install_identity_v1` verbatim, so the extension
 * cannot tell a copy from the original: identical `storage.local` means an
 * identical install id, an identical salt, an identical profile label, and no
 * window handle or profile directory a host could use to tell the two apart.
 * What a copy *cannot* carry is the other copy's future — each instance keeps
 * its own monotonic `report_seq` and mints its own `report_nonce` for every
 * sequence it allocates, so two live writers on one id eventually allocate the
 * same number and the host sees it under two different nonces. That collision
 * is the only positive evidence of cloning the protocol has, and it is observed
 * by the host, never here.
 *
 * The CTO decision of 2026-09-29 replaced an earlier rule that read a
 * *regression* as the signal. A report can arrive out of order, a send that
 * times out is retried, and a worker can restart between allocating a sequence
 * and sending it — none of which needs a second writer to explain, so a rule
 * built on ordering accused honest installs. What is evidence now is the
 * **pair**: one sequence under two different nonces is two allocations of one
 * number, which no single writer can produce, while a repeat of the same pair
 * is one allocation arriving twice.
 *
 * This file covers the extension's half:
 *
 * ① the sequence and its nonce are monotonic and random respectively, live in
 *    `storage.local`, are persisted **before** the message that carries them,
 *    and are *unknown* — never zero, never a fabricated nonce — when that
 *    storage cannot be read or no random source is available;
 * ② the repair mints a new install id and resets the sequence, and it is
 *    reachable *only* from the explicit user action: nothing on an automatic
 *    path rekeys a profile, because a restored backup or a renamed profile
 *    would then silently sever the wrong lineage;
 * ③ the popup shows the repair for a conflicted install and says plainly that
 *    the queued captures are still there and that old records keep the old id.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { getInstallIdentity, rekeyInstallIdentity } from '../lib/install-identity';
import {
  REPORT_SEQ_KEY,
  nextReportSeq,
  nextReportStamp,
  readReportSeq,
  resetReportSeq,
} from '../lib/report-seq';
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

  it('mints a nonce with every sequence and persists the pair as one value', async () => {
    const storage = fakeStorage();
    vi.stubGlobal('chrome', { storage: { local: storage } });

    const first = await nextReportStamp();
    expect(first).toEqual({ seq: 1, nonce: expect.stringMatching(UUID) });
    // The stored value is the *pair*, and it is written before the caller can
    // send anything: a message carries `seq` and `nonce` together or not at all,
    // so there is no moment at which a sequence is on the wire under a nonce
    // that was minted for a different one.
    expect(storage.values[REPORT_SEQ_KEY]).toEqual(first);

    const second = await nextReportStamp();
    expect(second?.seq).toBe(2);
    expect(second?.nonce).not.toBe(first?.nonce);
    expect(storage.values[REPORT_SEQ_KEY]).toEqual(second);
  });

  it('🔴 a worker that restarts after allocating resumes past the value it reserved', async () => {
    const storage = fakeStorage();
    vi.stubGlobal('chrome', { storage: { local: storage } });

    const reserved = await nextReportStamp();
    expect(reserved?.seq).toBe(1);
    // The worker dies here, before the message that would have carried this
    // stamp is built. A fresh worker is a fresh module — the send chain, the
    // in-memory state, all of it is gone; the counter is not, because it was
    // persisted before the send. Resuming *at* the reserved value would put one
    // sequence on the wire twice, which is the shape a copy has.
    vi.resetModules();
    const restarted = await import('../lib/report-seq');

    const next = await restarted.nextReportStamp();
    expect(next?.seq).toBe(2);
    expect(next?.nonce).not.toBe(reserved?.nonce);
    expect(storage.values[restarted.REPORT_SEQ_KEY]).toEqual(next);
  });

  it('reads the shape this key held before the nonce, and keeps its counter', async () => {
    // An install that ran the previous build holds a bare number: the sequence
    // is a real reading and is kept, and the next allocation mints a nonce for
    // the value it reserves. Nothing is re-issued.
    vi.stubGlobal('chrome', { storage: { local: fakeStorage({ [REPORT_SEQ_KEY]: 7 }) } });
    expect(await readReportSeq()).toBe(7);
    const stamp = await nextReportStamp();
    expect(stamp?.seq).toBe(8);
    expect(stamp?.nonce).toMatch(UUID);
  });

  it('🔴 unreadable storage is unknown, never zero', async () => {
    vi.stubGlobal('chrome', { storage: { local: undefined } });
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBeNull();
    expect(await nextReportStamp()).toBeNull();
  });

  it('🔴 a corrupt stored value is unknown, and is not silently overwritten', async () => {
    const storage = fakeStorage({ [REPORT_SEQ_KEY]: 'not a number' });
    vi.stubGlobal('chrome', { storage: { local: storage } });
    expect(await readReportSeq()).toBeNull();
    expect(await nextReportSeq()).toBeNull();
    expect(storage.set).not.toHaveBeenCalled();
  });

  it('🔴 no random source is unknown, not a nonce this side invented', async () => {
    // Without a nonce a sequence is not evidence, so there is nothing to send:
    // a fabricated one would be identical on both copies of a profile, which is
    // exactly the case this mechanism exists to detect.
    const storage = fakeStorage();
    vi.stubGlobal('chrome', { storage: { local: storage } });
    vi.stubGlobal('crypto', {});
    expect(await nextReportStamp()).toBeNull();
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

  it('stamps a fresh sequence and nonce on every status report and every delivery', async () => {
    vi.stubGlobal('chrome', { storage: { local: fakeStorage() } });
    const host = createSyntheticHost({ up: true });
    stubRuntime(host);

    await reportInstallStatus(statusBody());
    await deliver('deepseek-a.json', '{}', null);

    const status = host.requests().find((request) => request.type === 'status')!;
    const delivery = host.requests().find((request) => request.type === 'deliver')!;
    expect((status.status as Record<string, unknown>).report_seq).toBe(1);
    expect((status.status as Record<string, unknown>).report_nonce).toMatch(UUID);
    expect(delivery.report_seq).toBe(2);
    expect(delivery.report_nonce).toMatch(UUID);
    // Different allocations, different nonces: a nonce that repeated would make
    // two writers' frames indistinguishable from one writer's retry.
    expect((status.status as Record<string, unknown>).report_nonce)
      .not.toBe(delivery.report_nonce);
  });

  it('🔴 omits the sequence and the nonce together rather than inventing either', async () => {
    // A number with no nonce cannot be judged, and a nonce with no number names
    // nothing: the host refuses that pair as malformed, so an install with no
    // readable counter must send neither.
    vi.stubGlobal('chrome', { storage: { local: undefined } });
    const host = createSyntheticHost({ up: true });
    stubRuntime(host);

    await reportInstallStatus(statusBody());
    await deliver('deepseek-a.json', '{}', null);
    const status = host.requests().find((request) => request.type === 'status')!;
    const delivery = host.requests().find((request) => request.type === 'deliver')!;
    expect(Object.hasOwn(status.status as object, 'report_seq')).toBe(false);
    expect(Object.hasOwn(status.status as object, 'report_nonce')).toBe(false);
    expect(delivery.report_seq).toBeUndefined();
    expect(delivery.report_nonce).toBeUndefined();
  });

  it('🔴 two overlapping sends reserve distinct allocations, in ascending order', async () => {
    // Each native message is handled by its own host process, so two sends in
    // flight at once are two allocations racing on one stored counter: without
    // the shared chain both would read the same base and reserve the same
    // number, and the host would see one sequence twice under two nonces —
    // which is precisely the signature of a copied profile. The synthetic host
    // resolves the first delivery last, so a stamp taken at send time would
    // invert them.
    const storage = fakeStorage();
    vi.stubGlobal('chrome', { storage: { local: storage } });
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
    // second one could still be waiting on that is not the chain — its digest,
    // its storage read — so that "it has not been sent" is a statement about
    // the chain rather than about a microtask that has not run yet.
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(seen).toBe(1);
    expect(host.requests()).toHaveLength(0);
    releaseFirst!();
    await Promise.all([firstDelivery, secondDelivery]);

    const stamps = host
      .requests()
      .filter((request) => request.type === 'deliver')
      .map((request) => ({ seq: request.report_seq as number, nonce: request.report_nonce as string }));
    expect(stamps.map((stamp) => stamp.seq)).toEqual([1, 2]);
    expect(new Set(stamps.map((stamp) => stamp.nonce)).size).toBe(2);
    expect(stamps[1]).toEqual(storage.values[REPORT_SEQ_KEY]);
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
