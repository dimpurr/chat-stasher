import { describe, expect, it, vi } from 'vitest';
import { acquireSharedLease, backfillLeaseKey, type SharedLeaseState } from '../lib/backfill/shared-lease';

type Lease = { account: string; release(): Promise<void> };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

describe('account-scoped shared backfill leases', () => {
  it('shares one acquire for the same account and keeps another account independent', async () => {
    const leases = new Map<string, SharedLeaseState<Lease>>();
    const releaseA = vi.fn(async () => undefined);
    const releaseB = vi.fn(async () => undefined);
    const createA = vi.fn(async (): Promise<Lease> => ({ account: 'account-a', release: releaseA }));
    const createB = vi.fn(async (): Promise<Lease> => ({ account: 'account-b', release: releaseB }));

    const a1 = await acquireSharedLease(leases, backfillLeaseKey('claude', 'account-a'), createA);
    const a2 = await acquireSharedLease(leases, backfillLeaseKey('claude', 'account-a'), createA);
    const b1 = await acquireSharedLease(leases, backfillLeaseKey('claude', 'account-b'), createB);

    expect(createA).toHaveBeenCalledTimes(1);
    expect(createB).toHaveBeenCalledTimes(1);
    expect(a1?.account).toBe('account-a');
    expect(a2?.account).toBe('account-a');
    expect(b1?.account).toBe('account-b');

    await a1?.release();
    expect(releaseA).not.toHaveBeenCalled();
    await b1?.release();
    expect(releaseB).toHaveBeenCalledTimes(1);
    await a2?.release();
    expect(releaseA).toHaveBeenCalledTimes(1);
  });

  it('reacquires the same account after release completes', async () => {
    const leases = new Map<string, SharedLeaseState<Lease>>();
    const releasing = deferred<void>();
    const firstRelease = vi.fn(() => releasing.promise);
    const create = vi.fn(async (): Promise<Lease> => ({ account: 'account-a', release: firstRelease }));
    const key = backfillLeaseKey('claude', 'account-a');

    const first = await acquireSharedLease(leases, key, create);
    const release = first!.release();
    const reacquire = acquireSharedLease(leases, key, create);
    await Promise.resolve();
    expect(create).toHaveBeenCalledTimes(1);

    releasing.resolve();
    await release;
    const second = await reacquire;
    expect(create).toHaveBeenCalledTimes(2);
    expect(second?.account).toBe('account-a');
    await second?.release();
  });

  it('shares a rejected acquire, keeps other accounts independent, and retries the failed account', async () => {
    const leases = new Map<string, SharedLeaseState<Lease>>();
    const acquiring = deferred<Lease | null>();
    const failure = new Error('synthetic lease refusal');
    const createA = vi.fn(() => acquiring.promise);
    const releaseB = vi.fn(async () => undefined);
    const createB = vi.fn(async (): Promise<Lease> => ({ account: 'account-b', release: releaseB }));
    const keyA = backfillLeaseKey('claude', 'account-a');
    const first = acquireSharedLease(leases, keyA, createA);
    const second = acquireSharedLease(leases, keyA, createA);
    const other = await acquireSharedLease(leases, backfillLeaseKey('claude', 'account-b'), createB);
    const firstFailure = expect(first).rejects.toBe(failure);
    const secondFailure = expect(second).rejects.toBe(failure);

    expect(createA).toHaveBeenCalledTimes(1);
    expect(other?.account).toBe('account-b');
    expect(createB).toHaveBeenCalledTimes(1);

    acquiring.reject(failure);
    await Promise.all([firstFailure, secondFailure]);

    const releaseA = vi.fn(async () => undefined);
    const retry = await acquireSharedLease(leases, keyA, async () => ({ account: 'account-a', release: releaseA }));
    expect(retry?.account).toBe('account-a');
    await retry?.release();
    await other?.release();
    expect(releaseA).toHaveBeenCalledTimes(1);
    expect(releaseB).toHaveBeenCalledTimes(1);
  });

  it('keeps missing and unresolved accounts in platform scope', () => {
    expect(backfillLeaseKey('claude')).toBe(backfillLeaseKey('claude', ''));
    expect(backfillLeaseKey('claude', 'account-a')).not.toBe(backfillLeaseKey('claude', 'account-b'));
    expect(backfillLeaseKey('claude', 'account-a')).not.toBe(backfillLeaseKey('chatgpt', 'account-a'));
  });
});
