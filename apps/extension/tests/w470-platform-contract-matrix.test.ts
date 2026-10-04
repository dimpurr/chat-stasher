import { describe, expect, it } from 'vitest';
import {
  isCapturedFetchShape,
  isChatTraffic,
  matchesResponseShape,
  PLATFORMS,
  platformForTraffic,
  type ChatPlatform,
} from '../lib/contract';

const CAPTURED_AT = 1;

/**
 * W473 · A DeepSeek capture is bound to the current-session request that named
 * it: the query carries a chat_session_id and the body repeats the same nested
 * id, and isCapturedFetchShape refuses the body otherwise. The row's data does
 * not express that binding (it lives in the gate), so the matrix spells it out
 * here, beside the other declared facts it exercises.
 */
const BOUND_SESSION_ID = 'aaaa473e-0000-4000-8000-00000000000a';
const QUERY_ID_BINDINGS: Partial<Record<string, { queryKey: string; path: string }>> = {
  deepseek: { queryKey: 'chat_session_id', path: 'data.biz_data.chat_session.id' },
};

function bodyWithPaths(paths: readonly string[], leaf: unknown = 'x'): Record<string, unknown> {
  const body: Record<string, unknown> = {};
  for (const path of paths) {
    const parts = path.split('.');
    let current = body;
    for (const part of parts.slice(0, -1)) {
      const child = current[part];
      if (typeof child !== 'object' || child === null || Array.isArray(child)) {
        current[part] = {};
      }
      current = current[part] as Record<string, unknown>;
    }
    current[parts.at(-1)!] = leaf;
  }
  return body;
}

/** Point the bound body path at the same id the capture's query will carry. */
function bindQueryNamedSession(platform: ChatPlatform, body: Record<string, unknown>): void {
  const binding = QUERY_ID_BINDINGS[platform.id];
  if (!binding) return;
  const parts = binding.path.split('.');
  let current = body;
  for (const part of parts.slice(0, -1)) {
    const child = current[part];
    if (typeof child !== 'object' || child === null || Array.isArray(child)) {
      current[part] = {};
    }
    current = current[part] as Record<string, unknown>;
  }
  current[parts.at(-1)!] = BOUND_SESSION_ID;
}

function minimumResponse(platform: ChatPlatform): string {
  const shape = platform.responseShape;
  if (shape.encoding === 'text') {
    return (shape.requiredTextIncludes ?? []).join(' ');
  }
  const paths = [
    ...(shape.requiredPaths ?? []),
    ...((shape.requiredAnyPaths ?? []).slice(0, 1)),
  ];
  const body = {
    ...bodyWithPaths(paths),
    // An array path is satisfied by an EMPTY array: `[]` is a measurement.
    ...bodyWithPaths(shape.requiredArrayPaths ?? [], []),
  };
  bindQueryNamedSession(platform, body);
  return JSON.stringify(body);
}

function missingRequiredResponse(platform: ChatPlatform): string {
  const shape = platform.responseShape;
  if (shape.encoding === 'text') return 'x';
  const requiredPaths = shape.requiredPaths ?? [];
  return JSON.stringify(bodyWithPaths(requiredPaths.slice(1)));
}

function capture(platform: ChatPlatform, origin: string, path: string, method: string, text: string) {
  const pathname = path.startsWith('/') ? path : `/${path}`;
  const binding = QUERY_ID_BINDINGS[platform.id];
  const query = binding ? `?${binding.queryKey}=${BOUND_SESSION_ID}` : '';
  return {
    url: `${origin}${pathname}${query}`,
    method,
    status: 200,
    text,
    capturedAt: CAPTURED_AT,
  };
}

describe('W470 · declared platform contract matrix', () => {
  it.each(PLATFORMS.flatMap((platform) => platform.origins.map((origin) => ({ platform, origin }))))(
    '$platform.id accepts its declared origin, path, method, and minimum response shape ($origin)',
    ({ platform, origin }) => {
      const path = platform.pathHints[0]!;
      const method = platform.methods[0]!;
      const text = minimumResponse(platform);
      const candidate = capture(platform, origin, path, method, text);

      expect(platformForTraffic(candidate.url, method)?.id).toBe(platform.id);
      expect(matchesResponseShape(platform, text)).toBe(true);
      expect(isCapturedFetchShape(candidate)).toBe(true);
    },
  );

  it.each(PLATFORMS.flatMap((platform) => platform.origins.map((origin) => ({ platform, origin }))))(
    '$platform.id rejects a neighboring origin, method, and missing required field ($origin)',
    ({ platform, origin }) => {
      const path = platform.pathHints[0]!;
      const acceptedMethod = platform.methods[0]!;
      const text = minimumResponse(platform);
      const candidate = capture(platform, origin, path, acceptedMethod, text);
      const neighboringOrigin = `${origin}.invalid`;
      const wrongOriginCandidate = capture(platform, neighboringOrigin, path, acceptedMethod, text);
      const wrongMethod = 'OPTIONS';

      expect(platformForTraffic(wrongOriginCandidate.url, acceptedMethod)).toBeNull();
      expect(isCapturedFetchShape(wrongOriginCandidate)).toBe(false);

      expect(platformForTraffic(candidate.url, wrongMethod)).toBeNull();
      expect(isCapturedFetchShape({ ...candidate, method: wrongMethod })).toBe(false);

      const missingField = missingRequiredResponse(platform);
      expect(matchesResponseShape(platform, missingField)).toBe(false);
      expect(isCapturedFetchShape({ ...candidate, text: missingField })).toBe(false);
    },
  );

  it('does not promote unsupported transports or unrecognized paths to captures', () => {
    const platform = PLATFORMS[0]!;
    const origin = platform.origins[0]!;
    const validText = minimumResponse(platform);
    const unknownPath = '/unrecognized/opaque';
    const validMethod = platform.methods[0]!;

    expect(isChatTraffic(`${origin}${unknownPath}`, validMethod)).toBe(false);
    expect(platformForTraffic(`${origin}${unknownPath}`, validMethod)).toBeNull();
    expect(isCapturedFetchShape(capture(platform, origin, unknownPath, validMethod, validText))).toBe(false);

    for (const unsupportedUrl of [
      `ws://${new URL(origin).host}${platform.pathHints[0]}`,
      `wss://${new URL(origin).host}${platform.pathHints[0]}`,
    ]) {
      expect(isChatTraffic(unsupportedUrl, validMethod)).toBe(false);
      expect(platformForTraffic(unsupportedUrl, validMethod)).toBeNull();
      expect(isCapturedFetchShape({
        ...capture(platform, origin, platform.pathHints[0]!, validMethod, validText),
        url: unsupportedUrl,
      })).toBe(false);
    }

    // EventSource uses an HTTP(S) URL, so its transport identity is not present
    // in a capture payload. The active table's explicit opt-outs are the
    // contract-level proof that no row promotes EventSource or WebSocket data.
    expect(PLATFORMS.every((row) => row.eventSourceCapture !== true && row.webSocketCapture !== true)).toBe(true);
  });
});
