/**
 * W213 · EXT-4: the export file must name itself uniquely **and** with its
 * producing install.
 *
 * The topology this guards is the one `36-EXTENSION-TOPOLOGY.md` §1 states: one
 * user runs N browsers × K profiles, and *any* two of those installs can press
 * "export" within the same second. Until W213 the name was
 * `chat-stasher-export-<UTC-to-the-second>.jsonl`, so those two installs wrote
 * byte-equal names into one download directory and the browser resolved the
 * collision by renaming or overwriting — provenance lost and a file at risk
 * (W203 audit "Manual export filename", P0).
 *
 * The name since W213:
 * `chat-stasher-export-<UTC yyyymmddThhmmssZ>-<install_id short form>-<nonce>.jsonl`
 *  · the short install id (first 8 hex of the install identity) says which
 *    browser profile produced the file — it is stable, and that is exactly why
 *    it is *not* enough on its own: two exports by the same install, or by two
 *    copies of one profile that still share an install id (D4's later-comer
 *    regeneration has not run yet), need the per-export nonce to stay distinct;
 *  · the nonce is 6 lowercase hex from the platform's random source, drawn
 *    fresh per export so a name, once seen, is never written again;
 *  · an install identity that cannot be read omits the segment rather than
 *    inventing one — the escape hatch must not break because storage did — and
 *    the nonce still keeps the name unique;
 *  · a nonce that cannot be drawn is a refusal, not a degraded file name: the
 *    same standard `deliver` holds for `crypto` (lib/native-host.ts), applied
 *    to the export path the same way.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

const AT = Date.parse('2026-09-12T21:47:03.123Z');
const INSTALL = '7d3e9f21-1111-4222-8333-444444444444';
const INSTALL_B = '99ccab01-1111-4222-8333-444444444444';
const NONCE = 'a06f2b';

async function outbox() {
  return await import('../lib/outbox');
}

beforeEach(() => {
  vi.resetModules();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('W213 · the export file name', () => {
  it('🔴 carries the short install id and the nonce: chat-stasher-export-<stamp>-<install8>-<nonce>.jsonl', async () => {
    const ob = await outbox();
    expect(ob.exportFilename(AT, INSTALL, NONCE))
      .toBe('chat-stasher-export-20260912T214703Z-7d3e9f21-a06f2b.jsonl');
    // Midnight and single digits stay zero-padded, and the stamp is UTC.
    expect(ob.exportFilename(Date.parse('2026-01-02T03:04:05.000Z'), INSTALL, NONCE))
      .toBe('chat-stasher-export-20260102T030405Z-7d3e9f21-a06f2b.jsonl');
  });

  it('🔴 two profiles exporting in the same second cannot write the same name', async () => {
    const ob = await outbox();
    const a = ob.exportFilename(AT, INSTALL, NONCE);
    const b = ob.exportFilename(AT, INSTALL_B, NONCE);
    // Different installs: the install segment alone separates them.
    expect(a).not.toBe(b);
    // The same install (or a copied profile still sharing its id) exporting
    // twice in one second: only the nonce separates them.
    const c = ob.exportFilename(AT, INSTALL, 'b12345');
    expect(c).not.toBe(a);
    expect(c).not.toBe(b);
  });

  it('omits the install segment when the identity could be read — it never invents one', async () => {
    const ob = await outbox();
    // The escape hatch survives broken identity storage: no fabricated id, and
    // the nonce still keeps the name unique against every other export.
    expect(ob.exportFilename(AT, null, NONCE))
      .toBe('chat-stasher-export-20260912T214703Z-a06f2b.jsonl');
  });

  it('refuses a malformed nonce rather than naming a file the promise was not kept about', async () => {
    const ob = await outbox();
    expect(() => ob.exportFilename(AT, INSTALL, 'nothex!')).toThrow();
    expect(() => ob.exportFilename(AT, INSTALL, '')).toThrow();
    // 5 chars is not 6: the shape is the unambiguity, so it is policeable.
    expect(() => ob.exportFilename(AT, INSTALL, 'a06f2')).toThrow();
  });

  it('refuses an install id with nothing to shorten to 8 chars', async () => {
    const ob = await outbox();
    expect(() => ob.exportFilename(AT, 'a1', NONCE)).toThrow();
  });
});

describe('W213 · buildExportFile names the file the same way', () => {
  const entry = {
    sha256: '0000000000000000000000000000000000000000000000000000000000000000',
    name: 'chatgpt-aaaaaaaa-1111-4222-8333-444444444444.json',
    payload: '{"sessionId":"aaaaaaaa-1111-4222-8333-444444444444"}',
    bytes: 55,
    enqueuedAt: 1,
    attempts: 0,
    lastError: null,
    lastAttemptAt: null,
    state: 'pending' as const,
  };

  it('🔴 the file the user receives carries name and content from the same call', async () => {
    const ob = await outbox();
    const file = ob.buildExportFile([entry], AT, INSTALL, NONCE);
    expect(file.filename).toBe('chat-stasher-export-20260912T214703Z-7d3e9f21-a06f2b.jsonl');
    // The line is still the exact payload plus "\n" — the §8 contract that
    // makes an import line hash-equal a delivery is untouched by the name.
    expect(file.content).toBe(`${entry.payload}\n`);
  });
});

describe('W213 · the export nonce', () => {
  it('🔴 draws 6 lowercase hex per export, and two draws are not the same value', async () => {
    const ob = await outbox();
    const seen = new Set<string>();
    for (let i = 0; i < 4; i += 1) {
      const nonce = await ob.exportNonce();
      expect(nonce).toMatch(/^[0-9a-f]{6}$/);
      seen.add(nonce!);
    }
    // Not literally a uniqueness proof (6 bits of hex drawn at random),
    // but any repeated draw across four experiments means the source is a
    // constant — the failure the audit's "unique nonce" wording cares about.
    expect(seen.size).toBeGreaterThan(1);
  });

  it('is derived from the platform random source, so it can be pinned', async () => {
    const draw = (bytes: Uint8Array): void => {
      bytes.set([0xa0, 0x6f, 0x2b]);
    };
    vi.stubGlobal('crypto', { getRandomValues: draw });
    const ob = await outbox();
    expect(await ob.exportNonce()).toBe('a06f2b');
  });

  it('🔴 a random source that fails is a refusal (null), never a degraded file name', async () => {
    vi.stubGlobal('crypto', {
      getRandomValues: () => {
        throw new Error('no entropy');
      },
    });
    const ob = await outbox();
    expect(await ob.exportNonce()).toBeNull();
  });
});
