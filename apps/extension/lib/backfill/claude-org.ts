/**
 * 🔴 W31 · **Where the organization id comes from, on claude.ai.**
 *
 * ## Why this module exists at all
 * Every claude.ai request the backfill leg makes carries the organization id in
 * its path (`/api/organizations/<org>/chat_conversations/...`), and that id is
 * **not in the page URL**. So the leg cannot build a single URL — not the list,
 * not one conversation — until this value exists. A plan that guessed it, or
 * walked every organization it could find, would be reading a different
 * organization's history than the page the user is looking at. That is the one
 * thing this file is written to prevent.
 *
 * ## The order, and why it is this order
 *   1. **What the page's own requests already showed.** The live hook observes
 *      `/api/organizations/<org>/…` on this tab, so the org the user's page is
 *      really using is already in hand, and it costs nothing to remember. This is
 *      the only source that is *evidence about this page*, so it wins.
 *   2. **The `lastActiveOrg` cookie**, when the script can read it. The sources
 *      read it, which implies it is **not HttpOnly** on claude.ai — if it were,
 *      `document.cookie` could not see it and every source that reads it there
 *      would be broken. Recorded as an implication of the sources, not as a
 *      measured fact: nothing in this task opened a real claude.ai page.
 *   3. **`GET /api/organizations`**, once, and only when the first two answered
 *      nothing. Exactly **one** organization ⇒ use it. **Several and neither of
 *      the above** ⇒ `org-ambiguous`: a named halt, shown to the user, resolved
 *      by a human (or by the page later showing its own org), never by picking
 *      one. Iterating the organizations until one answers is exactly the
 *      behaviour that makes "your history" mean "whatever we found first".
 *
 * ## Who runs it
 * 🔴 `lib/backfill/claude-page.ts` — the page-side half, which is where the cookie
 * and a same-origin `fetch` live. It is reached by the background over the backfill
 * tab channel (`entrypoints/background.ts` asks when it registers a Claude target
 * or wakes one whose scope is still unresolved). W31 wrote this decision and left
 * that caller unwritten, which is why the review's first finding was *the resolver
 * is dead code*; the two files now exist as a pair and neither is usable alone.
 *
 * ## What this module does not do
 * No DOM, no `fetch`, no cookies API, no chrome.* API. It is a pure decision over
 * three inputs the caller has already collected, so the decision can be tested
 * without a browser — and so that the one request it needs is made by the caller
 * that owns the page context, through the same allowlist every other backfill
 * request goes through (`ScopeInPathSpec.resolvePath`).
 */

/** The cookie the sources read the active organization from. */
export const CLAUDE_ORG_COOKIE = 'lastActiveOrg';

/**
 * 🔴 The one shape `/api/organizations` may have for the resolver to read it: a
 * JSON **array** of records carrying a string `uuid`. Every other outcome is a
 * named refusal — an object with an `organizations` key inside, a list of ids
 * spelled differently, a login page's HTML. None of those is "you have one
 * organization", and returning `[]` for them would turn "we could not read it"
 * into "you have none".
 */
export function parseOrganizationsResponse(
  text: string,
): { ok: true; orgs: string[] } | { ok: false; detail: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'the organizations response is not JSON' };
  }
  if (!Array.isArray(parsed)) {
    return { ok: false, detail: 'the organizations response is not an array' };
  }
  const orgs: string[] = [];
  for (const entry of parsed) {
    if (!entry || typeof entry !== 'object') {
      return { ok: false, detail: 'an organization record is not an object' };
    }
    const uuid = (entry as Record<string, unknown>).uuid;
    if (typeof uuid !== 'string' || uuid.length === 0) {
      return { ok: false, detail: 'an organization record carries no non-empty uuid' };
    }
    // 🔴 W49b · The same predicate the request URL uses. A slug, a 32-hex
    //    title, or a dashed hex-word is not an organization id: listing it
    //    would let the resolver succeed with a value the engine then refuses.
    //    Skip it rather than failing the whole list, so a real uuid sitting
    //    next to garbage is not rejected.
    if (!isClaudeOrgId(uuid)) continue;
    orgs.push(uuid);
  }
  return { ok: true, orgs };
}

/**
 * Read the active organization out of a cookie header (`document.cookie`'s own
 * format). Returns null — never an empty string — when the cookie is absent or
 * carries an empty value: "the cookie said nothing" and "the cookie said the
 * organization is ''" are the same fact here, and neither is an organization.
 */
export function orgFromCookie(cookie: string): string | null {
  for (const part of cookie.split(';')) {
    const eq = part.indexOf('=');
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() !== CLAUDE_ORG_COOKIE) continue;
    const value = part.slice(eq + 1).trim();
    if (value.length === 0) return null;
    // Same predicate as the request URL. A cookie that is not an organization
    // id is "the cookie said nothing" — the endpoint still gets its turn —
    // never a successful resolution the engine will refuse.
    return isClaudeOrgId(value) ? value : null;
  }
  return null;
}

/**
 * 🔴 W49 · **Is this string an organization id, or something else sitting in
 * the field that must hold one?**
 *
 * Every source that names an organization produces the endpoint's `uuid` shape:
 * 8-4-4-4-12 hex (uppercase allowed), the same value the page puts in
 * `/api/organizations/<org>/…`. `'default'` is already this repository's
 * spelling for "the identifier could not be told", and the engine refuses it
 * before any request. A conversation title — what the identity heuristic
 * harvests from a conversation body's `name` — is not that shape, including
 * dashed hex-words (`cafe-babe`) that the previous "contains a dash and only
 * hex-or-dash" test accepted. Substituting one would address an organization
 * that does not exist.
 *
 * 🔴 This is not a guess about *which* organization an account has. It is a
 *    refusal to treat a string that cannot be settled as an organization id as
 *    if it were one. A value that is not this shape is the same fact as
 *    `'default'`: `org-unresolved`. All three sources (page URL, cookie,
 *    endpoint) use this predicate; a miss is a named refusal, not a silent
 *    `'default'` row the engine will then refuse.
 */
const CLAUDE_ORG_ID_RE =
  /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

export function isClaudeOrgId(value: string): boolean {
  return CLAUDE_ORG_ID_RE.test(value);
}

/**
 * 🔴 Read the organization out of **the page's own request URL**, i.e. one the
 * live hook already saw. This is what makes source 1 above possible, and it is
 * deliberately a pure function of a URL: nothing here remembers anything, so the
 * memory itself (which URL was seen most recently on this tab) stays where it
 * belongs, in the page-side caller.
 *
 * Only the exact prefix `/api/organizations/<one segment>/` is read. A URL that
 * merely contains the word is not one of this page's requests. A segment that
 * is not an organization id (a conversation title, the unresolved sentinel) is
 * not one either: returning it would write a non-organization into the field
 * that must hold one.
 */
export function orgFromRequestUrl(url: string): string | null {
  let pathname: string;
  try {
    pathname = new URL(url).pathname;
  } catch {
    return null;
  }
  const prefix = '/api/organizations/';
  if (!pathname.startsWith(prefix)) return null;
  const rest = pathname.slice(prefix.length);
  const slash = rest.indexOf('/');
  if (slash <= 0) return null;
  const org = rest.slice(0, slash);
  return isClaudeOrgId(org) ? org : null;
}

/**
 * What the caller knows about `GET /api/organizations` at the moment it asks.
 * Three states, and the third is not the second: "we did not need it" is not
 * "it failed".
 */
export type OrgEndpointReading =
  | { kind: 'not-asked' }
  /** The request itself did not complete (transport, HTTP status). Never read as "no organizations". */
  | { kind: 'failed'; detail: string }
  | { kind: 'text'; text: string };

export type OrgResolution =
  | { ok: true; org: string; source: 'page' | 'cookie' | 'endpoint' }
  /**
   * 🔴 Named refusals, never a guessed organization.
   *  · 'org-ambiguous'   — several organizations and nothing on the page said
   *                        which one. A human (or the page's own next request)
   *                        resolves this; this code does not.
   *  · 'org-unresolved'  — no organization could be named at all (the endpoint
   *                        answered with an empty array).
   *  · 'transport-error' — the one request this resolver needs did not complete.
   *                        Transient: the leg will ask again by itself. It is
   *                        **not** "you have no conversations".
   */
  | { ok: false; halt: 'org-ambiguous' | 'org-unresolved' | 'transport-error'; detail: string };

export interface OrgResolutionInput {
  /** The last organization this tab's own requests carried, or null if none was seen. */
  seen: string | null;
  /** The page's cookie header, verbatim. */
  cookie: string;
  /** What the caller knows about `GET /api/organizations`. */
  endpoint: OrgEndpointReading;
}

/**
 * Decide the organization. Pure; see the header for the order and its reasons.
 *
 * 🔴 The order is not an optimisation. Source 1 is evidence about **this page**;
 *    source 2 is a value the platform itself maintains per browser profile and
 *    can lag behind a tab that has switched organizations; source 3 is a list
 *    that may hold several. Each later source is weaker than the one before it,
 *    and the last one is only allowed to answer when it is unambiguous.
 */
export function resolveClaudeOrg(input: OrgResolutionInput): OrgResolution {
  if (input.seen !== null && isClaudeOrgId(input.seen)) {
    return { ok: true, org: input.seen, source: 'page' };
  }
  const fromCookie = orgFromCookie(input.cookie);
  if (fromCookie !== null) {
    return { ok: true, org: fromCookie, source: 'cookie' };
  }
  if (input.endpoint.kind === 'not-asked') {
    // The caller stopped before asking because it already had an answer; reaching
    // here means it did not. Say so rather than inventing one.
    return {
      ok: false,
      halt: 'org-unresolved',
      detail: 'no organization was seen on the page, none was in the cookie, and the organizations request was not made',
    };
  }
  if (input.endpoint.kind === 'failed') {
    return { ok: false, halt: 'transport-error', detail: input.endpoint.detail };
  }
  const parsed = parseOrganizationsResponse(input.endpoint.text);
  if (!parsed.ok) {
    // A body that is not the array of organizations is a shape fact, and the
    // person reading it needs the shape, not a guess about which org is meant.
    return { ok: false, halt: 'org-unresolved', detail: parsed.detail };
  }
  if (parsed.orgs.length === 0) {
    return { ok: false, halt: 'org-unresolved', detail: 'the organizations response listed no organization' };
  }
  if (parsed.orgs.length > 1) {
    return {
      ok: false,
      halt: 'org-ambiguous',
      detail: `the account has ${parsed.orgs.length} organizations and neither the page nor the cookie named one`,
    };
  }
  return { ok: true, org: parsed.orgs[0]!, source: 'endpoint' };
}

/**
 * 🔴 W31 · **The whole resolution, including the one request it may need.**
 *
 * `resolveClaudeOrg` above is the decision; this is the sequence a caller on the
 * page side actually runs, and the reason it is a function rather than three lines
 * at each call site is the **when**: `fetchOrganizations` may be called at most
 * once, and only when the two free sources answered nothing. A caller that fetched
 * first and asked afterwards would spend a request per tick on an account whose
 * page already says which organization it is — and, worse, would have a second
 * answer to reconcile with the first.
 *
 * 🔴 `fetchOrganizations` is a parameter rather than a `fetch` call made here: this
 *    module stays free of the network, and the caller that owns the page context is
 *    the one that knows the URL is on the allowlist (`ScopeInPathSpec.resolvePath`).
 *    A throw or a rejection from it is the `{kind:'failed'}` reading — the request
 *    did not complete — and it never becomes "no organizations".
 */
export async function resolveClaudeOrgOnPage(
  input: { seen: string | null; cookie: string },
  fetchOrganizations: () => Promise<string>,
): Promise<OrgResolution> {
  const settled = resolveClaudeOrg({ ...input, endpoint: { kind: 'not-asked' } });
  if (settled.ok) return settled;
  let text: string;
  try {
    text = await fetchOrganizations();
  } catch (err) {
    return { ok: false, halt: 'transport-error', detail: (err as Error).message };
  }
  return resolveClaudeOrg({ ...input, endpoint: { kind: 'text', text } });
}
