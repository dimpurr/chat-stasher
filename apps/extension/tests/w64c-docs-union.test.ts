/**
 * 🔴 W64c · **The one-re-decision rule is in the user-facing documents, and the
 * sentences that state it point at the code that implements it.**
 *
 * Why a test reads markdown at all. The merge that brought main's W59/W59b work
 * into this branch (`8f310ae`) kept this branch's prose wherever both sides had a
 * paragraph and so dropped main's additions, and **no gate in this repository
 * reads prose**: the citation lock checks only that the lines a citation points at
 * have not changed. A sentence that was never put back is invisible to it, and so
 * is a sentence put back pointing at the wrong code — which is exactly what the
 * re-review of that merge found (`nm/R64b-grok.log`, finding 3: the sentences
 * about a stop written by an *older* build are absent from `docs/privacy.md` and
 * `docs/threat-model.md`, and main's one-line comment on `haltReasonForStatus` is
 * absent from `engine.ts`).
 *
 * So two facts are pinned, per sentence:
 *
 *  1. the words exist in the document (whitespace-normalised, so a re-wrap is not
 *     a failure and a deletion is);
 *  2. the range that sentence cites is the code it is about — read off disk, not
 *     trusted from the sentence. A citation that resolves is not the same as a
 *     citation that resolves *to the right text*, which is the failure mode the
 *     whole `docs/citations.lock` mechanism exists to catch and cannot.
 *
 * Nothing here is about Kimi or Gemini; it is the documentation half of this
 * branch's two review findings.
 */

import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';

const ROOT = new URL('../../..', import.meta.url).pathname; // repository root

function doc(rel: string): string {
  return readFileSync(`${ROOT}${rel}`, 'utf8');
}

/** The document with every run of whitespace collapsed, so a wrap is not a fact. */
function flat(text: string): string {
  return text.replace(/\s+/g, ' ');
}

/**
 * The citation a sentence carries, read **out of the sentence**, not hard-coded.
 *
 * 🔴 A test that pinned the number instead would go red every time the cited code
 *    moves, and the tempting repair would be to paste the new number in — which tests
 *    nothing. What this file is for is that the sentence points at the *right code*,
 *    wherever that code currently sits.
 */
function citationAfter(
  rel: string,
  sentence: string,
  path: string,
): { start: number; end: number } {
  const text = flat(doc(rel));
  const at = text.indexOf(flat(sentence));
  expect(at, `the sentence is not in ${rel}: ${sentence.slice(0, 60)}…`).toBeGreaterThanOrEqual(0);
  const tail = text.slice(at + flat(sentence).length, at + flat(sentence).length + 240);
  // The sentence may close on more than one citation (`(a.ts:1-2`; `b.ts:3-4`)`), so the
  // one wanted is named rather than taken by position.
  const all = [...tail.matchAll(/`([A-Za-z0-9_./-]+\.ts):(\d+)(?:-(\d+))?`/g)];
  const found = all.find((m) => m[1] === path);
  expect(found, `no citation of ${path} follows the sentence: ${tail.slice(0, 120)}`).not.toBeUndefined();
  return { start: Number(found![2]), end: Number(found![3] ?? found![2]) };
}

/** The lines a cited path and range name, as text (1-based, end inclusive). */
function citedText(rel: string, start: number, end: number): string {
  const lines = doc(rel).split('\n');
  return lines.slice(start - 1, end).join('\n');
}

describe('W64c-1 · the re-decision of a stale stop is stated in the documents', () => {
  it('docs/privacy.md keeps the header field that names the build that spent the one re-decision', () => {
    // The storage table's `cs_backfill_v2` row: the field is written while a stored
    // stop is being re-decided, and it holds a version string and a timestamp —
    // nothing else, because the row is user-facing.
    expect(flat(doc('docs/privacy.md'))).toContain(
      flat('daily count, halt record, the record that a platform\'s id list had to be read again, and — while a stored stop is being re-decided — which build has already spent that one re-decision (a version string and a timestamp, nothing else).'),
    );
  });

  it('docs/privacy.md keeps the sentence about a record an earlier build left', () => {
    const text = flat(doc('docs/privacy.md'));
    expect(text).toContain(flat('once **this build** has recorded that answer the page is not asked again on every wake-up'));
    expect(text).toContain(flat('A record an **earlier** build left is re-asked once, and only once: a recorded judgement is that build\'s, not this one\'s, and a refusal that repeats is written back naming the build that saw it.'));
    // The bound is enforced rather than intended: the attempt is written down
    // before the request goes out.
    expect(text).toContain(flat('"Once" is enforced rather than intended: the attempt is recorded **before** the request goes out'));
  });

  it('docs/threat-model.md keeps its half of the same sentence', () => {
    const text = flat(doc('docs/threat-model.md'));
    expect(text).toContain(flat('"several organizations, no signal" that **this build** recorded is not asked again'));
    expect(text).toContain(flat('one an earlier build recorded is re-asked exactly once, since a judgement written by another build is not this one\'s'));
    expect(text).toContain(flat('the attempt is recorded in the scope\'s own progress header before the request goes out so a write that does not land cannot make it once per wake-up'));
  });

  it('the range those sentences cite is the code that spends the one re-decision', () => {
    // The citation that closes "… once **this build** has recorded that answer the page
    // is not asked again". What makes that true is the marker read at the top of
    // `scopeRetryDue` — the build stamp written into the header — so the cited range
    // must carry it.
    const { start, end } = citationAfter(
      'docs/privacy.md',
      'the page is not asked again on every wake-up — the answer is already known',
      'apps/extension/entrypoints/background.ts',
    );
    const scopeRetryDue = citedText('apps/extension/entrypoints/background.ts', start, end);
    expect(scopeRetryDue).toContain('haltRetried');
    expect(scopeRetryDue).toContain('scopeRetryDue');
  });

  it('the sentinel `default` is cited at the code that refuses it for this platform', () => {
    // The citation that closes "… `default` … is refused outright for this platform
    // rather than written into a path segment".
    const { start, end } = citationAfter(
      'docs/privacy.md',
      'is refused outright for this platform rather than written into a path segment',
      'apps/extension/lib/backfill/engine.ts',
    );
    const sentinel = citedText('apps/extension/lib/backfill/engine.ts', start, end);
    expect(sentinel).toContain('scopeInPath');
    expect(sentinel).toContain("'default'");
  });
});

describe('W64c-2 · `haltReasonForStatus` still carries its one-line summary', () => {
  it('the comment sits directly above the function, as it does on main', () => {
    // Main's line, dropped when this branch's own (longer) header comment was kept
    // in the merge. The long header is 150 lines above the function; without this
    // line the function has no summary of its own at all.
    const engine = doc('apps/extension/lib/backfill/engine.ts');
    expect(engine).toContain(
      '/** Classify a non-2xx: rate-limit family vs everything else. Both stop, but they leave different traces. */\nfunction haltReasonForStatus(',
    );
  });
});
