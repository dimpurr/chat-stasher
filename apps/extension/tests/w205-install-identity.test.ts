import { afterEach, describe, expect, it, vi } from 'vitest';
import { browserLabel, getInstallIdentity, INSTALL_IDENTITY_KEY, setProfileLabel } from '../lib/install-identity';

describe('W205 install identity', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('persists a stable id per browser profile and saves the user-provided profile label', async () => {
    const values: Record<string, unknown> = {};
    const storage = {
      get: vi.fn(async (query: Record<string, unknown>) => {
        const key = Object.keys(query)[0]!;
        return { [key]: values[key] };
      }),
      set: vi.fn(async (items: Record<string, unknown>) => Object.assign(values, items)),
    };
    vi.stubGlobal('chrome', { storage: { local: storage } });

    const first = await getInstallIdentity();
    expect(first.install_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(values[INSTALL_IDENTITY_KEY]).toEqual(first);
    const named = await setProfileLabel('Work');
    expect(named).toEqual({ ...first, profile_label: 'Work' });
    expect(values[INSTALL_IDENTITY_KEY]).toEqual(named);
    expect(await getInstallIdentity()).toEqual(named);
  });

  it('maps known browser user agents to readable browser labels', () => {
    expect(browserLabel('Mozilla/5.0 Edg/120')).toBe('Edge');
    expect(browserLabel('Mozilla/5.0 Chrome/120')).toBe('Chrome');
    expect(browserLabel('Mozilla/5.0 Chrome/120', 'Apple Computer, Inc.')).toBe('Arc');
  });
});
