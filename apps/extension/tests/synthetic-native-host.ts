/**
 * A synthetic chat-stasher native host for the tests.
 *
 * It implements exactly §6 of `contracts/nativehost-protocol.md` — one request
 * in, one response out — and nothing else. It is NOT production code and is not
 * part of any build: `vitest.config.ts` only collects `tests/**\/*.test.ts`.
 *
 * 🔴 Why one shared copy instead of a hand-rolled stub per test file: the whole
 *    point of the suite is that the extension and the host agree on the wire
 *    shape. Two independently invented stubs would let both drift while every
 *    test stayed green.
 *
 * The host is deliberately strict about the two things §1 turns on: it echoes
 * the `request_id` it was handed and it answers `duplicate` for payload bytes
 * it has already sealed (content addressing, §7). Tests that need a *broken*
 * host (a missing field, a mismatched id) write their own mock instead — see
 * w2-native-host.test.ts.
 */

import { NATIVE_HOST_NAME } from '../lib/native-host';

export interface SyntheticDelivery {
  name: string;
  payload: string;
  sha256: string;
  requestId: string;
  status: 'stored' | 'duplicate';
}

export interface SyntheticHostOptions {
  /** false ⇒ every call throws, the way a missing host manifest behaves. */
  up?: boolean;
  /** Non-null ⇒ every `deliver` gets this nack instead of an ack. */
  nack?: { kind: string; retryable: boolean; detail: string } | null;
  /** Answers `hello` with `ok:false` when false (the host exists but is not usable). */
  helloOk?: boolean;
  stage?: string;
  machine?: string;
  hostVersion?: string;
}

export interface SyntheticHost {
  deliveries: SyntheticDelivery[];
  /** How many `hello` requests the extension actually sent. */
  helloCount(): number;
  /**
   * Pass this as `browser.runtime.sendNativeMessage`. Written as an arrow so it
   * keeps working when the browser object calls it as `runtime.sendNativeMessage(...)`
   * — a method would see the wrong `this`.
   */
  sendNativeMessage: (host: string, message: unknown) => Promise<unknown>;
  /** Delivery names, e.g. `chatgpt-<id>.json` — §6.2's `name`. */
  names(): string[];
  /** Session ids decoded from the delivered bundles. */
  sessionIds(): string[];
}

export async function sha256Of(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

export function createSyntheticHost(options: SyntheticHostOptions = {}): SyntheticHost {
  const up = options.up ?? true;
  const stage = options.stage ?? '/tmp/chat-stasher-stage-fixture';
  const machine = options.machine ?? 'fixture-machine';
  const hostVersion = options.hostVersion ?? '0.3.0';

  const deliveries: SyntheticDelivery[] = [];
  const sealed = new Set<string>();
  let hellos = 0;

  return {
    deliveries,
    helloCount: () => hellos,
    names: () => deliveries.map((d) => d.name),
    sessionIds: () => deliveries.map((d) => {
      try {
        return (JSON.parse(d.payload) as { sessionId?: string }).sessionId ?? '(none)';
      } catch {
        return '(unparseable)';
      }
    }),
    async sendNativeMessage(host: string, message: unknown): Promise<unknown> {
      if (!up) throw new Error('Specified native messaging host not found.');
      if (host !== NATIVE_HOST_NAME) throw new Error(`Unknown host ${host}`);
      const msg = message as Record<string, unknown>;

      if (msg.type === 'hello') {
        hellos += 1;
        if (options.helloOk === false) {
          return {
            protocol: 1, type: 'nack', request_id: null,
            kind: 'config', retryable: false,
            detail: 'no [native_host] stage configured',
          };
        }
        return {
          protocol: 1, type: 'hello', ok: true,
          host_version: hostVersion, machine, stage,
        };
      }

      if (msg.type !== 'deliver') throw new Error(`unexpected request type ${String(msg.type)}`);
      const requestId = String(msg.request_id);
      const payload = String(msg.payload);
      const sha256 = String(msg.sha256);

      if (options.nack) {
        return {
          protocol: 1, type: 'nack', request_id: requestId,
          kind: options.nack.kind, retryable: options.nack.retryable,
          detail: options.nack.detail,
        };
      }
      // §6.2: the host recomputes the hash over the payload bytes.
      if (await sha256Of(payload) !== sha256) {
        return {
          protocol: 1, type: 'nack', request_id: requestId,
          kind: 'integrity', retryable: true, detail: 'sha256 does not match payload',
        };
      }
      const duplicate = sealed.has(sha256);
      sealed.add(sha256);
      const status = duplicate ? 'duplicate' : 'stored';
      deliveries.push({ name: String(msg.name), payload, sha256, requestId, status });
      return {
        protocol: 1, type: 'ack', request_id: requestId,
        status, sha256, shard: `0001-${String(msg.name)}`,
      };
    },
  };
}
