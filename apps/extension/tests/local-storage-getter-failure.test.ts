import { afterEach, describe, expect, it, vi } from 'vitest';
import { localStorageArea, type StorageArea } from '../lib/local-storage';

const storageArea = (): StorageArea => ({
  get: async () => ({}),
  set: async () => {},
});

function throwingGetter(name: 'storage' | 'local'): object {
  return Object.defineProperty({}, name, {
    get() {
      throw new Error(`${name} getter failed`);
    },
  });
}

describe('localStorageArea getter failures', () => {
  afterEach(() => vi.unstubAllGlobals());

  it.each(['storage', 'local'] as const)(
    'falls back to chrome when browser %s getter throws',
    (getter) => {
      const chromeLocal = storageArea();
      const browser = getter === 'storage'
        ? throwingGetter('storage')
        : { storage: throwingGetter('local') };
      vi.stubGlobal('browser', browser);
      vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

      expect(localStorageArea()).toBe(chromeLocal);
    },
  );

  it('returns null without throwing when both candidates have throwing getters', () => {
    vi.stubGlobal('browser', throwingGetter('storage'));
    vi.stubGlobal('chrome', { storage: throwingGetter('local') });

    expect(() => localStorageArea()).not.toThrow();
    expect(localStorageArea()).toBeNull();
  });
});
