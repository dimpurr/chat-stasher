/**
 * 🔴 W31c · **The page-side half of the Claude organization resolver.**
 *
 * ## Why this is a module and not six lines inside the content script
 * `lib/backfill/claude-org.ts` is the *decision* — pure, no DOM, no fetch — and the
 * content script is the only place that has the two things the decision needs
 * beyond itself: the cookie, and a same-origin `fetch`. W31 wrote the decision and
 * left that second half unwritten, so nothing called it (the review's first
 * finding: *the resolver is dead code*). Writing it inline in
 * `entrypoints/dw-bridge.content.ts` would have made it reachable and **untestable
 * at the same time** — the content script boots against a real page, and a test
 * that re-implements its glue is testing the re-implementation.
 *
 * So the glue lives here, with three injected facts and no globals of its own, and
 * the content script keeps only the wiring: hand it the message, hand it the URLs
 * the page's own requests carried, and hand the fetch channel the scope it says
 * this page is allowed to address.
 *
 * ## 🔴 The two states this object keeps, and why neither is a guess
 *  · `seen` — the organization of the **most recent request this page made**. It is
 *    the resolver's first source because it is evidence about *this page* rather
 *    than about the account (see claude-org.ts's header for the whole order);
 *  · `allowed` — the organization this page's backfill requests may address, which
 *    is what `checkBackfillRequest` compares the `{org}` path segment against.
 *    Without it every claude.ai request is refused **at the page**
 *    (`scopePathMatches` matches nothing against `null`), which is the other half
 *    of the same finding: the background could have resolved the organization
 *    perfectly and every request would still have been refused on the way out.
 *
 * Neither is ever `''`, neither is ever written to storage, and both are per tab —
 * they are facts about a page. The scope the *run* uses is persisted by the
 * background, under its own key.
 */

import { getPlatformByOrigin } from '../contract';
import { backfillPlanFor } from './enumerate';
import { orgFromRequestUrl, resolveClaudeOrgOnPage, type OrgResolution } from './claude-org';
import { isClaudeOrgRequest, serveBackfillFetch, type FetchLike } from './tab-port';

export interface ClaudePageScopeDeps {
  /** The page's own origin, and the origin every request below is checked against. */
  pageOrigin: string;
  /** The content script's fetch: same-origin, credentials included, allowlist included. */
  fetchImpl: FetchLike;
  /** `document.cookie` at the moment it is asked, or null where there is no document. */
  readCookie: () => string | null;
}

export interface ClaudePageScope {
  /** Remember the organization one of this page's own requests carried. */
  rememberRequest(url: string): void;
  /** The organization this page's backfill requests may address. Null = nothing is allowed. */
  allowedScope(): string | null;
  /**
   * Answer background's question, or **null** when the message is not that question
   * (the caller then offers it to the next listener — this channel dispatches by
   * message type, exactly like `handleBackfillMessage`).
   */
  handleMessage(message: unknown): Promise<OrgResolution> | null;
}

export function createClaudePageScope(deps: ClaudePageScopeDeps): ClaudePageScope {
  let seen: string | null = null;
  let allowed: string | null = null;

  /**
   * 🔴 The resolution-only path, **as the plan itself declares it** — never spelled
   * out here. A second copy of `/api/organizations` in this file is a second thing
   * to keep true, and the one that would drift is the one the allowlist checks.
   */
  const resolvePath = backfillPlanFor('claude')?.scopeInPath?.resolvePath ?? null;

  /**
   * 🔴 **The one request the resolver may spend**, through the very same allowlist
   * as every other request on this channel (`serveBackfillFetch` — same origin,
   * platform table, the plan's own declared path, GET, no query, no body).
   *
   * 🔴 **A non-2xx is a failed request, not an answer.** Throwing here is what makes
   *    a 5xx or a login redirect the resolver's transient `transport-error` — "we
   *    did not finish asking" — instead of a body that fails to parse and would be
   *    recorded as "this account has no organization". Those two facts are one
   *    request apart and must never be the same fact.
   */
  const fetchOrganizations = async (): Promise<string> => {
    if (resolvePath === null) throw new Error('the claude plan declares no resolution path');
    const reply = await serveBackfillFetch(`${deps.pageOrigin}${resolvePath}`, deps.pageOrigin, deps.fetchImpl);
    if (!reply.ok) throw new Error(`the organizations request did not complete: ${reply.error}`);
    if (reply.status < 200 || reply.status > 299) {
      throw new Error(`the organizations request answered HTTP ${reply.status}`);
    }
    return reply.text;
  };

  return {
    rememberRequest(url: string): void {
      const org = orgFromRequestUrl(url);
      if (org === null) return;
      seen = org;
      // 🔴 The page's own request is also the strongest evidence about **what this
      //    page may address**: it is the organization the user's own page is really
      //    working in, observed rather than asked for.
      allowed = org;
    },

    allowedScope(): string | null {
      return allowed;
    },

    handleMessage(message: unknown): Promise<OrgResolution> | null {
      if (!isClaudeOrgRequest(message)) return null;
      // 🔴 Only claude.ai has an organization to resolve. A question arriving on
      //    another platform's page is refused by name rather than answered with a
      //    cookie that happens to be there.
      if (getPlatformByOrigin(deps.pageOrigin)?.id !== 'claude') {
        return Promise.resolve({
          ok: false,
          halt: 'org-unresolved',
          detail: 'this page is not claude.ai, so it has no organization to resolve',
        });
      }
      // The cookie is read **per ask** and kept in no variable: a tab that has
      // switched organizations must not be answered from a copy taken before.
      return resolveClaudeOrgOnPage(
        { seen, cookie: deps.readCookie() ?? '' },
        fetchOrganizations,
      ).then((resolved) => {
        // 🔴 The answer is also what this page will allow its own backfill requests
        //    to address: the organization the run was started with and the
        //    organization the page says it is using must be the same value, or the
        //    request is refused rather than sent.
        if (resolved.ok) allowed = resolved.org;
        return resolved;
      });
    },
  };
}
