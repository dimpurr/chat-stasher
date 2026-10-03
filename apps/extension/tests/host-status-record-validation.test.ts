import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { memoryStore } from '../lib/backfill/store';

const AT = 1_700_000_000_000;

beforeEach(() => {
  vi.resetModules();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Preserve NaN/Infinity through the test store read, as structured storage can. */
function nonFiniteTimestampStore(key: string, record: Record<string, unknown>) {
  const store = memoryStore({ [key]: record });
  store.load = async (requestedKey) => requestedKey === key ? record : null;
  return store;
}

describe('persisted host status records', () => {
  it.each([
    ['negative', -1],
    ['NaN', Number.NaN],
    ['positive infinity', Number.POSITIVE_INFINITY],
    ['negative infinity', Number.NEGATIVE_INFINITY],
    ['string', '1700000000000'],
    ['missing', undefined],
  ])('rejects a %s timestamp', async (_label, at) => {
    const { HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const record = { at, ok: true };
    const store = typeof at === 'number' && !Number.isFinite(at)
      ? nonFiniteTimestampStore(HOST_STATUS_KEY, record)
      : memoryStore({ [HOST_STATUS_KEY]: record });

    expect(await loadHostStatus(store)).toBeNull();
  });

  it.each([
    ['missing timestamp', { ok: true }],
    ['non-boolean ok', { at: AT, ok: 'true' }],
    ['non-string reason', { at: AT, ok: false, reason: 7 }],
    ['non-string kind', { at: AT, ok: false, kind: null }],
    ['non-string detail', { at: AT, ok: false, detail: {} }],
    ['non-string machine', { at: AT, ok: true, machine: 3 }],
    ['non-string stage', { at: AT, ok: true, stage: null }],
    ['non-string hostVersion', { at: AT, ok: true, hostVersion: false }],
    ['non-string lastKnownStage', { at: AT, ok: false, lastKnownStage: 4 }],
  ])('rejects malformed core or optional field: %s', async (_label, record) => {
    const { HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const store = memoryStore({ [HOST_STATUS_KEY]: record });

    expect(await loadHostStatus(store)).toBeNull();
  });

  it('accepts documented status metadata when each present value is a string', async () => {
    const { HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const expected = {
      at: 0,
      ok: false,
      reason: 'malformed-response',
      kind: 'io',
      detail: 'synthetic host failure',
      machine: 'synthetic-machine',
      stage: '/synthetic/stage',
      hostVersion: '0.0.0-synthetic',
      lastKnownStage: '/synthetic/stage',
    };
    const store = memoryStore({ [HOST_STATUS_KEY]: expected });

    expect(await loadHostStatus(store)).toEqual(expected);
  });

  it('keeps the last known stage when a controlled hello fails after success', async () => {
    const { checkHost, HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const responses: unknown[] = [
      { protocol: 1, type: 'hello', ok: true, host_version: '0.0.0-synthetic', machine: 'synthetic-machine', stage: '/synthetic/stage' },
      { protocol: 1, type: 'hello', ok: false, host_version: '0.0.0-synthetic', machine: 'synthetic-machine', stage: '/different/stage' },
    ];
    const runtime = {
      id: 'synthetic-extension-id',
      sendNativeMessage(_host: unknown, _message: unknown, callback: (response: unknown) => void) {
        callback(responses.shift());
      },
    };
    vi.stubGlobal('browser', { runtime });
    vi.stubGlobal('chrome', { runtime });
    const store = memoryStore();

    const success = await checkHost(store, { now: AT });
    expect(success.lastKnownStage).toBe('/synthetic/stage');

    const failed = await checkHost(store, { now: AT + 1 });
    expect(failed.ok).toBe(false);
    expect(failed.lastKnownStage).toBe('/synthetic/stage');
    expect((await loadHostStatus(store))?.lastKnownStage).toBe('/synthetic/stage');
  });

  it('does not erase a known stage when replacing a malformed persisted failure record', async () => {
    const { checkHost, HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const store = memoryStore({
      [HOST_STATUS_KEY]: {
        at: AT,
        ok: false,
        reason: 'timeout',
        detail: 7,
        lastKnownStage: '/synthetic/known-stage',
      },
    });

    expect(await loadHostStatus(store)).toBeNull();
    const replacement = await checkHost(store, { now: AT + 1 });

    expect(replacement.ok).toBe(false);
    expect(replacement.lastKnownStage).toBe('/synthetic/known-stage');
    expect((await loadHostStatus(store))?.lastKnownStage).toBe('/synthetic/known-stage');
  });
});

describe('persisted host pause records', () => {
  it.each([
    ['negative', -1],
    ['NaN', Number.NaN],
    ['positive infinity', Number.POSITIVE_INFINITY],
    ['string', '1700000000000'],
    ['missing', undefined],
  ])('rejects a %s timestamp', async (_label, at) => {
    const { HOST_PAUSE_KEY, loadHostPause } = await import('../lib/host-status');
    const record = { at, reason: 'host-unavailable' };
    const store = typeof at === 'number' && !Number.isFinite(at)
      ? nonFiniteTimestampStore(HOST_PAUSE_KEY, record)
      : memoryStore({ [HOST_PAUSE_KEY]: record });

    expect(await loadHostPause(store)).toBeNull();
  });

  it.each([
    ['missing timestamp', { reason: 'host-unavailable' }],
    ['missing reason', { at: AT }],
    ['non-string reason', { at: AT, reason: false }],
    ['non-string detail', { at: AT, reason: 'host-unavailable', detail: 2 }],
  ])('rejects malformed pause record: %s', async (_label, record) => {
    const { HOST_PAUSE_KEY, loadHostPause } = await import('../lib/host-status');
    const store = memoryStore({ [HOST_PAUSE_KEY]: record });

    expect(await loadHostPause(store)).toBeNull();
  });

  it('accepts an optional string detail and the zero timestamp edge', async () => {
    const { HOST_PAUSE_KEY, loadHostPause } = await import('../lib/host-status');
    const expected = { at: 0, reason: 'host-unavailable', detail: 'synthetic timeout' };
    const store = memoryStore({ [HOST_PAUSE_KEY]: expected });

    expect(await loadHostPause(store)).toEqual(expected);
  });
});
