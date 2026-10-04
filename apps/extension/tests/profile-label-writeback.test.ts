import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

describe('profile label write-back', () => {
  beforeEach(() => vi.resetModules());
  afterEach(() => vi.unstubAllGlobals());

  async function setupStorage(options: {
    ignoreProfileLabelWrite?: boolean;
  } = {}) {
    const values: Record<string, unknown> = {};
    let ignoreProfileLabelWrite = false;
    const storage = {
      get: vi.fn(async (query: Record<string, unknown>) => {
        const key = Object.keys(query)[0]!;
        return { [key]: values[key] };
      }),
      set: vi.fn(async (items: Record<string, unknown>) => {
        if (ignoreProfileLabelWrite && 'profile_label' in (items.cs_install_identity_v1 as object)) return;
        Object.assign(values, items);
      }),
    };
    vi.stubGlobal('chrome', { storage: { local: storage } });
    const identity = await import('../lib/install-identity');
    return {
      values,
      storage,
      identity,
      ignoreProfileLabelWrite: () => { ignoreProfileLabelWrite = options.ignoreProfileLabelWrite ?? false; },
    };
  }

  it('rejects a successful no-op write and keeps the cached label unchanged', async () => {
    const { values, identity, ignoreProfileLabelWrite } = await setupStorage({ ignoreProfileLabelWrite: true });
    const current = await identity.getInstallIdentity();
    ignoreProfileLabelWrite();

    await expect(identity.setProfileLabel('Work')).rejects.toThrow(/persist/i);
    expect(values[identity.INSTALL_IDENTITY_KEY]).toEqual(current);
    expect(await identity.getInstallIdentity()).toEqual(current);
  });

  it('rejects a storage write failure and keeps the cached label unchanged', async () => {
    const { values, storage, identity } = await setupStorage();
    const current = await identity.getInstallIdentity();
    storage.set.mockRejectedValueOnce(new Error('synthetic storage write rejection'));

    await expect(identity.setProfileLabel('Work')).rejects.toThrow('synthetic storage write rejection');
    expect(values[identity.INSTALL_IDENTITY_KEY]).toEqual(current);
    expect(await identity.getInstallIdentity()).toEqual(current);
  });

  it('trims labels, caps them at 80 characters, and returns a confirmed round trip', async () => {
    const { values, identity } = await setupStorage();
    const initial = await identity.getInstallIdentity();
    const label = `  ${'A'.repeat(81)}  `;

    const updated = await identity.setProfileLabel(label);

    expect(updated).toEqual({ ...initial, profile_label: 'A'.repeat(80) });
    expect(values[identity.INSTALL_IDENTITY_KEY]).toEqual(updated);
    expect(await identity.getInstallIdentity()).toEqual(updated);
  });
});
