/**
 * W65 · Perplexity's conversation list keys its items by `slug`, not by the
 * `thread_id` C27 invented — and the two other numbers the response carries are
 * not denominators.
 *
 * ## What this file exists to stop changing back
 *
 *  1. **The id field is `slug`.** C27's parser read `thread_id`, a name no source
 *     ever recorded: R26 inferred it from three "build the request yourself"
 *     implementations, and W28's fixture asserted `thread_id` and `slug` held the
 *     same string. The live endpoint has **no `thread_id` key at all**, so the
 *     list leg halted on every run before reading a single conversation.
 *  2. **`uuid` is not the id and is not a fallback.** Perplexity items carry
 *     `slug`, `uuid` and `context_uuid` as three separate strings. The reference
 *     builds both the conversation URL and the content route as `slug || uuid`
 *     (Echoes 8.3.1, `perplexity.ts:getChatUrl` / `:fetchContent`), so when the
 *     two diverge the page URL's `/search/<segment>` carries the slug — and the
 *     live leg names a file after that segment. Reading `uuid` here would file one
 *     conversation under two names. A missing `slug` is therefore a `halt`, and
 *     deliberately **not** a silent fall back to `uuid`.
 *  3. **`total_threads` is not a total and `has_next_page` is not read.**
 *     `total_threads` came back as **99 against a list of 4**, identical on every
 *     row, so `total` stays `null` — a denominator must not be invented from a
 *     placeholder. `has_next_page` is real, and using it would change the engine's
 *     enumeration branch rather than this parser's, so it is left for its own
 *     change.
 *
 * ## Where the field names come from
 *
 * A live probe on **2026-09-23**: 4 read-only `POST`s to
 * `/rest/thread/list_ask_threads?version=2.18&source=default`, issued from the
 * logged-in page's own context over raw CDP (`127.0.0.1:9222`), body built by
 * `PERPLEXITY_PLAN`'s own `listPost.body`. Scripts and the full table:
 * `/Users/dimpurr/scratch/DimLifeS/chat-stasher/nm/drive/w65-probe{,2}.mjs` and
 * `/Users/dimpurr/scratch/DimLifeS/chat-stasher/nm/W65-OUT.md`.
 *
 * 🔴 Every fixture below is **synthetic** and hand-written from the field names
 *    and types the probe observed. No request goes to perplexity.ai, there is no
 *    logged-in state, and no real conversation, account or id appears here.
 */

import { describe, expect, it } from 'vitest';
import { extractSessionId } from '../lib/contract';
import { parsePerplexityListPage } from '../lib/backfill/enumerate';

const ORIGIN = 'https://www.perplexity.ai';

/** The thread's identity as the page URL carries it: the slug. */
const SLUG = 'how-do-i-rotate-a-secret-1aBcDeFg';
/** The other id on the same record. A DIFFERENT string on purpose — this is what `slug`-not-`uuid` pins. */
const THREAD_UUID = 'b7c2f0e3-4a51-4d8f-9c31-0f2a6e5d7b90';

/**
 * One record of the list response, with the field set and the JSON types the
 * probe observed on 2026-09-23. `thread_id` is absent because the endpoint does
 * not have that key — 36 keys came back and this was not among them.
 *
 * Deliberately included and deliberately unread: `has_next_page` (boolean) and
 * `total_threads` (number, and a placeholder rather than a count).
 */
function liveItem(slug: string): Record<string, unknown> {
  return {
    slug,
    uuid: THREAD_UUID,
    context_uuid: '3f9d1c44-77be-4a0e-8a11-6c5d2b9e4f70',
    title: `synthetic-${slug}`,
    last_query_datetime: '2025-03-25T09:14:02.000Z',
    query_count: 1,
    thread_number: 0,
    unread: false,
    has_next_page: false,
    total_threads: 99,
    answer_preview: 'synthetic preview',
    display_model: 'synthetic-model',
    mode: 'concise',
    sources: [],
    social_info: {},
    crons: null,
  };
}

/** The list response: a top-level array, exactly as the probe received it. */
function livePage(slugs: string[]): string {
  return JSON.stringify(slugs.map(liveItem));
}

/** Just the ids the parser produced, or `null` when it refused the page. */
function idsOrNull(text: string): string[] | null {
  const parsed = parsePerplexityListPage(text);
  return parsed.ok ? parsed.page.ids : null;
}

/** The refusal detail, or `null` when the page parsed. */
function detailOrNull(text: string): string | null {
  const parsed = parsePerplexityListPage(text);
  return parsed.ok ? null : parsed.detail;
}

describe('W65-1 · the id field is the one the endpoint actually sends', () => {
  it('a page of live-shaped items parses, and its ids are the `slug` values', () => {
    // 🔴 The whole bug: on the shipped parser this page is
    //    `{ok:false, detail:'... no string `thread_id`'}`, because the key it
    //    reads is not on the record. A live account's entire history halted here.
    expect(parsePerplexityListPage(livePage([SLUG, 'why-is-the-sky-blue-9zYx8WvU']))).toEqual({
      ok: true,
      page: { ids: [SLUG, 'why-is-the-sky-blue-9zYx8WvU'], total: null },
    });
  });

  it('the C27 name `thread_id` is not accepted as the id', () => {
    // 🔴 The name was the bug. An item that spells the id `thread_id` and
    //    carries no `slug` must NOT parse — otherwise the parser would keep
    //    accepting a shape the endpoint never sends, and a real field rename
    //    would go unnoticed behind it.
    expect(parsePerplexityListPage(JSON.stringify([{ thread_id: SLUG }])).ok).toBe(false);
    expect(idsOrNull(JSON.stringify([{ thread_id: SLUG }]))).toBeNull();
  });
});

describe('W65-2 · `uuid` is a different id, and never a fallback', () => {
  it('an item with `uuid` but no `slug` halts, and the detail names `slug`', () => {
    const text = JSON.stringify([{ uuid: THREAD_UUID, context_uuid: 'x', has_next_page: false }]);
    expect(parsePerplexityListPage(text).ok).toBe(false);
    // 🔴 The message has to name the field that was actually missing, or the next
    //    reader of the ledger chases the same wrong key this change removed.
    expect(detailOrNull(text)).toContain('slug');
    // ...and it must not be the old sentence, which named a key the API has not got.
    expect(detailOrNull(text)).not.toContain('thread_id');
    // 🔴 And the `uuid` must not have been used in its place. The parse refused,
    //    so `idsOrNull` is null — normalised to `[]` here so the assertion says
    //    what it means: no id at all came out of a record whose slug is missing,
    //    and in particular not the uuid.
    expect(idsOrNull(text) ?? []).not.toContain(THREAD_UUID);
    expect(idsOrNull(text) ?? []).toHaveLength(0);
  });

  it('a malformed record halts the whole page: it is not skipped, and no partial page comes back', () => {
    // 🔴 One bad record stops the page — it is never skipped, and the good
    //    records' ids are never handed back as if they were the whole page.
    const mixed = JSON.stringify([liveItem(SLUG), null, liveItem('why-is-the-sky-blue-9zYx8WvU')]);
    expect(parsePerplexityListPage(mixed).ok).toBe(false);
    expect(idsOrNull(mixed)).toBeNull();
    // 🔴 And the halt names the record that was actually malformed. On the
    //    shipped parser this page stops on the FIRST item instead, for a key the
    //    API never sends — so the detail below is what separates "we refused
    //    because the page is not the shape we know" from "we never got as far as
    //    looking at it".
    expect(detailOrNull(mixed)).toBe('perplexity thread item is not an object');
  });
});

describe('W65-3 · the two numbers on the record are not denominators', () => {
  it('`total_threads` and `has_next_page` are read by neither the ids nor `total`', () => {
    // `total_threads` returned 99 against a list of 4, identical on every row.
    // `total` must stay null: a placeholder written as a denominator is the
    // invented-total failure this repo refuses everywhere else.
    const parsed = parsePerplexityListPage(livePage([SLUG]));
    expect(parsed).toEqual({ ok: true, page: { ids: [SLUG], total: null } });
    // `has_next_page` is real and its use would move the engine's stopping rule,
    // so it must not leak into the parse result under any name.
    expect(Object.keys(parsed.ok ? parsed.page : {})).toEqual(['ids', 'total']);
  });
});

describe('W65-4 · the list id and the page URL are one value', () => {
  it('the id the parser returns is the segment the conversation URL carries', () => {
    // The W28 invariant, re-pinned on the real field name: the live leg reads
    // `/search/<segment>` off the page URL and the backfill leg reads the list.
    // For one thread those must be the same string, or the debt never settles.
    const listedId = idsOrNull(livePage([SLUG]))?.[0];
    expect(listedId).toBe(SLUG);
    const fromUrl = extractSessionId(
      `${ORIGIN}/rest/thread/${SLUG}?with_parent_info=true&version=2.18&source=default`,
      JSON.stringify({ entries: [] }),
      `${ORIGIN}/search/${SLUG}`,
    );
    expect(fromUrl).toBe(listedId);
  });
});
