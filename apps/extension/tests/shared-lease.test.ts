import { describe, expect, it, vi } from 'vitest';
import { acquireSharedLease, backfillLeaseKey, type SharedLeaseState } from '../lib/backfill/shared-lease';

type Lease = { account: string; release(): Promise<void> };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
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

  it('keeps missing and unresolved accounts in platform scope', () => {
    expect(backfillLeaseKey('claude')).toBe(backfillLeaseKey('claude', ''));
    expect(backfillLeaseKey('claude', 'account-a')).not.toBe(backfillLeaseKey('claude', 'account-b'));
    expect(backfillLeaseKey('claude', 'account-a')).not.toBe(backfillLeaseKey('chatgpt', 'account-a'));
  });
});
