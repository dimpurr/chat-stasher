import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const KEY = 'cs_report_seq_v1';
const UUID_A = '11111111-1111-4111-8111-111111111111';
const UUID_B = '22222222-2222-4222-8222-222222222222';

function fakeStorage(initial: Record<string, unknown> = {}) {
  const values: Record<string, unknown> = { ...initial };
  return {
    values,
    get: vi.fn(async (query: Record<string, unknown>) => {
      const key = Object.keys(query)[0]!;
      return { [key]: values[key] };
    }),
    set: vi.fn(async (items: Record<string, unknown>) => {
      Object.assign(values, items);
    }),
  };
}

function stubCrypto(...nonces: string[]): void {
  const next = vi.fn(() => nonces.shift() ?? UUID_B);
  vi.stubGlobal('crypto', { randomUUID: next });
}

async function loadReportSeq() {
  vi.resetModules();
  return import('../lib/report-seq');
}

beforeEach(() => {
  vi.resetModules();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('report sequence storage edges', () => {
  it('leaves malformed saved stamps untouched and returns unknown', async () => {
    const invalid = [
      'not a stamp',
      -1,
      Number.NaN,
      Number.POSITIVE_INFINITY,
      { seq: 1.5, nonce: UUID_A },
      { seq: 1, nonce: '' },
      { seq: 1, nonce: 'contains spaces' },
      [],
    ];

    for (const saved of invalid) {
      const storage = fakeStorage({ [KEY]: saved });
      vi.stubGlobal('browser', { storage: { local: storage } });
      const reportSeq = await loadReportSeq();

      expect(await reportSeq.readReportSeq()).toBeNull();
      expect(await reportSeq.nextReportStamp()).toBeNull();
      expect(storage.set, JSON.stringify(saved)).not.toHaveBeenCalled();
      vi.unstubAllGlobals();
    }
  });

  it('does not overflow a valid saved sequence or overwrite its evidence', async () => {
    const original = { seq: Number.MAX_SAFE_INTEGER, nonce: UUID_A };
    const storage = fakeStorage({ [KEY]: original });
    vi.stubGlobal('browser', { storage: { local: storage } });
    stubCrypto(UUID_B);
    const reportSeq = await loadReportSeq();

    expect(await reportSeq.nextReportStamp()).toBeNull();
    expect(storage.values[KEY]).toEqual(original);
    expect(storage.set).not.toHaveBeenCalled();
  });

  it('treats a storage read rejection as unknown and does not write', async () => {
    const storage = fakeStorage();
    storage.get.mockRejectedValue(new Error('synthetic read failure'));
    vi.stubGlobal('browser', { storage: { local: storage } });
    stubCrypto(UUID_A);
    const reportSeq = await loadReportSeq();

    expect(await reportSeq.readReportSeq()).toBeNull();
    expect(await reportSeq.nextReportStamp()).toBeNull();
    expect(storage.set).not.toHaveBeenCalled();
  });

  it('returns unknown after a storage write rejection', async () => {
    const storage = fakeStorage();
    storage.set.mockRejectedValue(new Error('synthetic write failure'));
    vi.stubGlobal('browser', { storage: { local: storage } });
    stubCrypto(UUID_A);
    const reportSeq = await loadReportSeq();

    expect(await reportSeq.nextReportStamp()).toBeNull();
    expect(storage.values[KEY]).toBeUndefined();
  });

  it('returns unknown when the persisted pair does not read back', async () => {
    const storage = fakeStorage();
    storage.get.mockImplementation(async (query) => {
      const key = Object.keys(query)[0]!;
      return { [key]: undefined };
    });
    vi.stubGlobal('browser', { storage: { local: storage } });
    stubCrypto(UUID_A);
    const reportSeq = await loadReportSeq();

    expect(await reportSeq.nextReportStamp()).toBeNull();
    expect(storage.set).toHaveBeenCalledWith({ [KEY]: { seq: 1, nonce: UUID_A } });
  });

  it('does not reserve a sequence when randomness is unavailable', async () => {
    const storage = fakeStorage();
    vi.stubGlobal('browser', { storage: { local: storage } });
    vi.stubGlobal('crypto', {});
    const reportSeq = await loadReportSeq();

    expect(await reportSeq.nextReportStamp()).toBeNull();
    expect(storage.set).not.toHaveBeenCalled();
  });

  it('serializes concurrent reservations into distinct durable pairs', async () => {
    const storage = fakeStorage();
    vi.stubGlobal('browser', { storage: { local: storage } });
    stubCrypto(UUID_A, UUID_B);
    const reportSeq = await loadReportSeq();

    const stamps = await Promise.all([
      reportSeq.nextReportStamp(),
      reportSeq.nextReportStamp(),
    ]);

    expect(stamps).toEqual([
      { seq: 1, nonce: UUID_A },
      { seq: 2, nonce: UUID_B },
    ]);
    expect(storage.values[KEY]).toEqual(stamps[1]);
  });
});
