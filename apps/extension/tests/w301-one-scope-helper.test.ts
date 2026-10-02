/**
 * 🔴 W301 · **One helper builds and compares every ChatGPT scope.**
 *
 * A ChatGPT scope is a small on-disk and wire format: `chatgpt:<workspace>`,
 * where the workspace is a raw id, an explicit `fp1:<hmac>` marker, or one of
 * the unknown-workspace sentinels. The format used to be written out
 * independently in `entrypoints/background.ts` and
 * `lib/backfill/chatgpt-scope-migration.ts` as well as the canonical
 * `lib/backfill/chatgpt-workspace.ts`; the copies agreed by luck, and a review
 * (W299d) asked for a single construction path. They now agree by construction:
 * every builder and predicate lives in `chatgpt-workspace.ts`, and this guard
 * makes the single path a checked fact rather than a convention.
 *
 * ## What it checks, and why it is a source scan rather than a unit test
 *
 * The property is about *where text is written*, so no behavioural test can
 * state it: a re-inlined `` `chatgpt:${x}` `` produces the same values as the
 * helper call and would pass any round-trip assertion. So this reads every
 * source file under `apps/extension` (excluding the suite itself and `e2e/`,
 * which keep literal scopes on purpose — a fixture that spelled its scope the
 * way production now must would stop exercising the migration's raw inputs) and
 * fails if any **string literal** outside the helper contains the scope
 * vocabulary: `chatgpt:`, `fp1:`, or the `!workspace-` sentinel marker.
 *
 * Comments and identifiers are not string literals, so the prose in
 * `coverage-read.ts`, `day-slow.ts` and `types.ts` that names the shape, and the
 * `chatgpt:` object key in `recapture.ts`, are outside the check by construction
 * — not by an allow-list that would rot. The comparison vocabulary lives in the
 * helper as predicates, so a re-inlined `startsWith('chatgpt:')` is caught
 * exactly as a re-inlined construction is.
 *
 * ## The second case
 *
 * A guard that scanned for a string the codebase had renamed everywhere would
 * pass while checking nothing. The second case therefore asserts the helper
 * still writes all three markers, so the guard cannot go green by the needles
 * disappearing.
 */

import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const ROOT = new URL('..', import.meta.url).pathname; // apps/extension
const HELPER = join(ROOT, 'lib/backfill/chatgpt-workspace.ts');

/** The scope vocabulary a construction or comparison outside the helper would write. */
const NEEDLES = ['chatgpt:', 'fp1:', '!workspace-'];

/** Every non-test, non-e2e `.ts`/`.tsx` source file in the extension. */
function sourceFiles(dir = ROOT, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    if (['node_modules', '.wxt', '.output', 'tests', 'e2e', '.git'].includes(name)) continue;
    const abs = join(dir, name);
    if (statSync(abs).isDirectory()) sourceFiles(abs, out);
    else if (/\.tsx?$/.test(name)) out.push(abs);
  }
  return out;
}

/**
 * The body of every string literal in `src`, with comments removed and everything
 * else discarded. A literal that spans lines (a template literal) is returned
 * whole, so a scope built across lines is still one body.
 */
function stringLiterals(src: string): string[] {
  const found: string[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i]!;
    if (c === '/' && src[i + 1] === '/') {
      while (i < src.length && src[i] !== '\n') i++;
      continue;
    }
    if (c === '/' && src[i + 1] === '*') {
      i += 2;
      while (i < src.length && !(src[i] === '*' && src[i + 1] === '/')) i++;
      i += 2;
      continue;
    }
    if (c === "'" || c === '"' || c === '`') {
      const quote = c;
      i++;
      let body = '';
      while (i < src.length) {
        const ch = src[i]!;
        if (ch === '\\') { body += ch + (src[i + 1] ?? ''); i += 2; continue; }
        if (ch === quote) { i++; break; }
        body += ch;
        i++;
      }
      found.push(body);
      continue;
    }
    i++;
  }
  return found;
}

function read(abs: string): string {
  return readFileSync(abs, 'utf8');
}

describe('W301-ONE-SCOPE-HELPER · ChatGPT scopes are built and compared in one place', () => {
  it('🔴 writes no scope literal outside chatgpt-workspace.ts', () => {
    const offenders: string[] = [];
    for (const abs of sourceFiles()) {
      if (abs === HELPER) continue;
      for (const literal of stringLiterals(read(abs))) {
        for (const needle of NEEDLES) {
          if (literal.includes(needle)) {
            offenders.push(`${relative(ROOT, abs)}: ${JSON.stringify(literal)}`);
          }
        }
      }
    }
    expect(offenders).toEqual([]);
  });

  it('the helper still writes every scope marker, so the guard is not vacuous', () => {
    const literals = stringLiterals(read(HELPER)).join('\n');
    for (const needle of NEEDLES) expect(literals).toContain(needle);
  });
});
