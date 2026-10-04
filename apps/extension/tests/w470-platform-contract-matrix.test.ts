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
 * A body carrying `paths`, each leaf written as `leaf`.
 *
 * 🔴 W422 · The leaf is a parameter because the gate reads its two required lists
 *    differently: a scalar satisfies `requiredPaths`, only an array satisfies
 *    `requiredArrayPaths`. Writing `'x'` for every path would build a kimi minimum body
 *    of `{}`, and `matchesResponseShape` would refuse it for the right reason while the
 *    row's "accepts its declared shape" case went red — the opposite of a real drift.
 */
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
  return JSON.stringify(body);
}

function missingRequiredResponse(platform: ChatPlatform): string {
  const shape = platform.responseShape;
  if (shape.encoding === 'text') return 'x';
  // 🔴 W422 · BOTH required lists are read, in declaration order, so dropping the last
  //    entry leaves a body the row refuses whichever of the two keys it actually uses.
  //    `requiredPaths` alone gives an array-only row — kimi and perplexity both — nothing
  //    to drop, so its missing-path case would be an empty body refused for the wrong
  //    reason. Each surviving array path keeps the `[]` that satisfies it above.
  const requiredPaths = shape.requiredPaths ?? [];
  const requiredArrayPaths = shape.requiredArrayPaths ?? [];
  const keptArrayPaths = requiredPaths.length > 0 ? requiredArrayPaths : requiredArrayPaths.slice(1);
  return JSON.stringify({
    ...bodyWithPaths(requiredPaths.slice(1)),
    ...bodyWithPaths(keptArrayPaths, []),
  });
}

function capture(platform: ChatPlatform, origin: string, path: string, method: string, text: string) {
  const pathname = path.startsWith('/') ? path : `/${path}`;
  return {
    url: `${origin}${pathname}`,
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
