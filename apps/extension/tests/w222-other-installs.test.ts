import { afterEach, describe, expect, it, vi } from 'vitest';
import { otherInstalls } from '../lib/native-host';
import { withI18n } from './i18n-harness';

function installHost(answer: (request: Record<string, unknown>) => unknown) {
  const requests: Array<Record<string, unknown>> = [];
  const fake: any = {
    runtime: {
      id: 'w222-other-installs-test',
      lastError: undefined,
      sendNativeMessage: (_name: string, request: Record<string, unknown>) => {
        requests.push(request);
        return Promise.resolve(answer(request));
      },
    },
  };
  const browser = withI18n(fake);
  vi.stubGlobal('browser', browser);
  vi.stubGlobal('chrome', browser);
  return requests;
}

afterEach(() => vi.unstubAllGlobals());

describe('EXT-7 · archive-wide other-install count', () => {
  it('sends only this install id and accepts a measured positive count', async () => {
    const requests = installHost((request) => ({
      protocol: 1, type: 'other_installs', ok: true,
      request_id: request.request_id, count: 2,
    }));
    const result = await otherInstalls('11111111-1111-4111-8111-111111111111');
    expect(result).toEqual({ ok: true, count: 2 });
    expect(requests).toHaveLength(1);
    expect(requests[0]).toMatchObject({
      protocol: 1, type: 'other_installs',
      install_id: '11111111-1111-4111-8111-111111111111',
    });
    expect(Object.keys(requests[0]!).sort()).toEqual(['install_id', 'protocol', 'request_id', 'type']);
  });

  it('keeps an archive that has no other installs distinct from an unreadable count', async () => {
    installHost((request) => ({
      protocol: 1, type: 'other_installs', ok: true,
      request_id: request.request_id, count: 0,
    }));
    expect(await otherInstalls('11111111-1111-4111-8111-111111111111')).toEqual({ ok: true, count: 0 });

    installHost(() => ({
      protocol: 1, type: 'nack', request_id: null,
      kind: 'io', retryable: true, detail: 'archive read failed',
    }));
    expect(await otherInstalls('11111111-1111-4111-8111-111111111111')).toEqual({ ok: false });
  });

  it('rejects malformed counts and responses for another request', async () => {
    installHost((request) => ({
      protocol: 1, type: 'other_installs', ok: true,
      request_id: request.request_id, count: -1,
    }));
    expect(await otherInstalls('11111111-1111-4111-8111-111111111111')).toEqual({ ok: false });

    installHost(() => ({ protocol: 1, type: 'other_installs', ok: true, request_id: 'different', count: 1 }));
    expect(await otherInstalls('11111111-1111-4111-8111-111111111111')).toEqual({ ok: false });
  });
});
