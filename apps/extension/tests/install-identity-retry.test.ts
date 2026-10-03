import { afterEach, describe, expect, it, vi } from 'vitest';

type FailurePoint = 'initial-read' | 'write' | 'read-back';

function syntheticStorage(failurePoint: FailurePoint | null = null) {
  const values: Record<string, unknown> = {};
  let failAt: FailurePoint | null = failurePoint;
  let readCount = 0;
  const storage = {
    values,
    get: vi.fn(async (query: Record<string, unknown>) => {
      readCount += 1;
      if ((failAt === 'initial-read' && readCount === 1)
        || (failAt === 'read-back' && readCount === 2)) {
        throw new Error(`synthetic ${failAt} failure`);
      }
      const key = Object.keys(query)[0]!;
      return { [key]: values[key] };
    }),
    set: vi.fn(async (items: Record<string, unknown>) => {
      if (failAt === 'write') throw new Error('synthetic write failure');
      Object.assign(values, items);
    }),
    recover() {
      failAt = null;
    },
  };
  return storage;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('install identity initialization retry', () => {
  it.each(['initial-read', 'write', 'read-back'] as const)(
    'retries after a transient %s failure instead of caching the rejection', async (failurePoint) => {
      vi.resetModules();
      const storage = syntheticStorage(failurePoint);
      vi.stubGlobal('chrome', { storage: { local: storage } });
      const { getInstallIdentity, INSTALL_IDENTITY_KEY } = await import('../lib/install-identity');

      await expect(getInstallIdentity()).rejects.toThrow(`synthetic ${failurePoint} failure`);
      storage.recover();

      const identity = await getInstallIdentity();
      expect(identity.install_id).toMatch(/^[0-9a-f-]{36}$/);
      expect(storage.values[INSTALL_IDENTITY_KEY]).toEqual(identity);
    },
  );

  it('preserves and refuses a malformed persisted identity', async () => {
    vi.resetModules();
    const malformed = { install_id: '', browser: 'Chrome', profile_label: null };
    const storage = syntheticStorage();
    Object.assign(storage.values, { cs_install_identity_v1: malformed });
    vi.stubGlobal('chrome', { storage: { local: storage } });
    const { getInstallIdentity } = await import('../lib/install-identity');

    await expect(getInstallIdentity()).rejects.toThrow('install identity record unreadable');
    expect(storage.values.cs_install_identity_v1).toBe(malformed);
    expect(storage.set).not.toHaveBeenCalled();
  });
});
