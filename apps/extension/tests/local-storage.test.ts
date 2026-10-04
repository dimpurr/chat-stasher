import { afterEach, describe, expect, it, vi } from 'vitest';
import { localStorageArea, type StorageArea } from '../lib/local-storage';

const storageArea = (): StorageArea => ({
  get: async () => ({}),
  set: async () => {},
});

describe('localStorageArea', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('prefers browser storage when browser and chrome are both available', () => {
    const browserLocal = storageArea();
    const chromeLocal = storageArea();
    vi.stubGlobal('browser', { storage: { local: browserLocal } });
    vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

    expect(localStorageArea()).toBe(browserLocal);
  });

  it('uses chrome storage when browser storage is absent', () => {
    const chromeLocal = storageArea();
    vi.stubGlobal('browser', undefined);
    vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

    expect(localStorageArea()).toBe(chromeLocal);
  });

  it('falls back to chrome when browser is only partially shaped', () => {
    const chromeLocal = storageArea();
    vi.stubGlobal('browser', { storage: {} });
    vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

    expect(localStorageArea()).toBe(chromeLocal);
  });

  it.each([
    ['missing get', { set: async () => {} }],
    ['missing set', { get: async () => ({}) }],
    ['non-callable get', { get: {}, set: async () => {} }],
    ['non-callable set', { get: async () => ({}), set: false }],
  ])('falls back to chrome when browser storage has %s', (_case, browserLocal) => {
    const chromeLocal = storageArea();
    vi.stubGlobal('browser', { storage: { local: browserLocal } });
    vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

    expect(localStorageArea()).toBe(chromeLocal);
  });

  it.each([
    ['missing get', { set: async () => {} }],
    ['missing set', { get: async () => ({}) }],
    ['non-callable get', { get: {}, set: async () => {} }],
    ['non-callable set', { get: async () => ({}), set: false }],
  ])('returns null when chrome storage has %s and browser is absent', (_case, chromeLocal) => {
    vi.stubGlobal('browser', undefined);
    vi.stubGlobal('chrome', { storage: { local: chromeLocal } });

    expect(localStorageArea()).toBeNull();
  });

  it('returns null when neither global has a storage area', () => {
    vi.stubGlobal('browser', undefined);
    vi.stubGlobal('chrome', undefined);

    expect(localStorageArea()).toBeNull();
  });

  it('returns null without throwing for nullish or partial globals', () => {
    vi.stubGlobal('browser', null);
    vi.stubGlobal('chrome', { storage: null });

    expect(localStorageArea()).toBeNull();
  });
});
