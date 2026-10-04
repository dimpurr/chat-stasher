export type ReleasableLease = { release(): Promise<void> };

export type SharedLeaseState<T extends ReleasableLease> = {
  holders: number;
  ready: Promise<T | null>;
  releasing?: Promise<void>;
};

/** Keep one shared lease per platform/account scope in this extension instance. */
export function backfillLeaseKey(platform: string, accountId?: string): string {
  return JSON.stringify([platform, accountId ?? '']);
}

/** Coalesce same-scope callers while keeping different account scopes independent. */
export async function acquireSharedLease<T extends ReleasableLease>(
  leases: Map<string, SharedLeaseState<T>>,
  key: string,
  create: () => Promise<T | null>,
): Promise<T | null> {
  const previous = leases.get(key);
  if (previous?.releasing) {
    await previous.releasing.catch(() => undefined);
    return acquireSharedLease(leases, key, create);
  }
  if (previous) {
    previous.holders += 1;
    const lease = await previous.ready;
    return lease ? { ...lease, release: releaseOnce(leases, key, previous, lease.release) } : null;
  }

  const shared: SharedLeaseState<T> = { holders: 1, ready: Promise.resolve(null) };
  leases.set(key, shared);
  let lease: T | null;
  try {
    shared.ready = create();
    lease = await shared.ready;
  } catch (error) {
    if (leases.get(key) === shared) leases.delete(key);
    throw error;
  }
  if (!lease) {
    if (leases.get(key) === shared) leases.delete(key);
    return null;
  }
  return { ...lease, release: releaseOnce(leases, key, shared, lease.release) };
}

function releaseOnce<T extends ReleasableLease>(
  leases: Map<string, SharedLeaseState<T>>,
  key: string,
  shared: SharedLeaseState<T>,
  hostRelease: () => Promise<void>,
): () => Promise<void> {
  let released = false;
  return async () => {
    if (released) return;
    released = true;
    shared.holders -= 1;
    if (shared.holders === 0) {
      shared.releasing = hostRelease().finally(() => {
        if (leases.get(key) === shared) leases.delete(key);
      });
      await shared.releasing;
    }
  };
}
