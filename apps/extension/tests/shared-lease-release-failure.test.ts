import { describe, expect, it, vi } from 'vitest';
import { acquireSharedLease, backfillLeaseKey, type SharedLeaseState } from '../lib/backfill/shared-lease';

type Lease = { account: string; generation: number; release(): Promise<void> };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

describe('shared lease release failures', () => {
  it('retries a waiting same-scope acquire after final release rejects', async () => {
    const leases = new Map<string, SharedLeaseState<Lease>>();
    const keyA = backfillLeaseKey('claude', 'account-a');
    const keyB = backfillLeaseKey('claude', 'account-b');
    const releaseFailure = new Error('synthetic release failure');
    const releasing = deferred<void>();
    const releaseFirst = vi.fn(() => releasing.promise);
    const releaseSecond = vi.fn(async () => undefined);
    const releaseOther = vi.fn(async () => undefined);
    const createA = vi.fn()
      .mockResolvedValueOnce({ account: 'account-a', generation: 1, release: releaseFirst })
      .mockResolvedValue({ account: 'account-a', generation: 2, release: releaseSecond });
    const createB = vi.fn(async (): Promise<Lease> => ({ account: 'account-b', generation: 1, release: releaseOther }));

    const first = await acquireSharedLease(leases, keyA, createA);
    const staleSharedState = leases.get(keyA);
    const release = first!.release();
    const waiting = acquireSharedLease(leases, keyA, createA);
    const other = await acquireSharedLease(leases, keyB, createB);

    expect(createA).toHaveBeenCalledTimes(1);
    expect(other).toMatchObject({ account: 'account-b', generation: 1 });
    await other?.release();
    expect(releaseOther).toHaveBeenCalledTimes(1);

    releasing.reject(releaseFailure);
    await expect(release).rejects.toBe(releaseFailure);

    const recovered = await waiting;
    expect(createA).toHaveBeenCalledTimes(2);
    expect(recovered).toMatchObject({ account: 'account-a', generation: 2 });
    expect(recovered).not.toBe(first);
    expect(leases.get(keyA)).not.toBe(staleSharedState);

    const afterSettlement = await acquireSharedLease(leases, keyA, createA);
    expect(createA).toHaveBeenCalledTimes(2);
    expect(afterSettlement?.generation).toBe(2);

    await recovered?.release();
    expect(releaseSecond).not.toHaveBeenCalled();
    await afterSettlement?.release();
    expect(releaseSecond).toHaveBeenCalledTimes(1);
    expect(leases.has(keyA)).toBe(false);
  });
});
