/**
 * 🔴 W613 · **The refusals in front of the coordination gateway, pinned.**
 *
 * `lib/backfill/coordination.ts` sits in front of every backfill request
 * (`entrypoints/background.ts`, `coordinatedHttp`): it answers which
 * coordination bucket a URL belongs to, and it **fails closed** — a request it
 * cannot place is not sent. `tests/w212-coordination.test.ts` covers the routes
 * every plan declares, the final "request is not declared" throw, and already
 * pins the gemini query refusal at one URL (`rpcids=unplanned`, :38-39); unpinned were
 * the platform refusal and the query refusal's neighbouring outcomes. Those are
 * the sentences a future edit is most likely to break without anything else going red:
 *
 *  · **no plan for the platform** (`backfillPlanFor`'s miss). Without it, a
 *    platform id that is not in the table would be classified against nothing;
 *    with it, the refusal names the platform, because a missing table row and a
 *    missing route are different facts and say different things to whoever reads
 *    the halt.
 *  · **query not declared on the path both segments share** (gemini's, the one
 *    plan whose `listPath === detailPath`). Path-only dispatch cannot tell those
 *    two RPCs apart — that is what `formSegmentFor` exists for — so a URL on the
 *    shared path whose query matches neither segment's pinned set is refused with
 *    a sentence about the *query*, not about the route. That difference is the
 *    whole point of the arm, so both the refusal and its neighbouring outcomes
 *    are pinned here.
 *
 * ## Why the placeholder rules are pinned through claude rather than directly
 *
 * `templatePathMatches` is private to coordination.ts, and the templates it
 * matches are the ones a real plan declares, so it is exercised the way it runs:
 * by classifying a URL on `CLAUDE_PLAN`, the only plan in the table with a
 * `scopeInPath`. The edge that matters is that a `{…}` placeholder matches
 * exactly one **non-empty** segment and every literal segment is compared
 * character for character — a doubled slash mid-path (or a trailing slash at
 * the end) keeps the segment count and so isolates the non-empty rule from the
 * count rule, which is the only way to tell the two apart from outside.
 *
 * Every pathname below is synthetic. No fixture is read from disk, no request
 * leaves the process, and `new URL` is the only thing that parses a string here.
 */

import { describe, expect, it } from 'vitest';
import { coordinationSegmentForRequest } from '../lib/backfill/coordination';
import {
  CHATGPT_LIST_PATH,
  CLAUDE_DETAIL_PATH_TEMPLATE,
  CLAUDE_LIST_PATH_TEMPLATE,
  CLAUDE_PLAN,
  DEEPSEEK_LIST_PATH,
  GEMINI_PLAN,
} from '../lib/backfill/enumerate';

const CLAUDE = 'https://claude.ai';

describe('W613 · coordination request classification', () => {
  it('names the platform it has no plan for, before it looks at the route', () => {
    // A path a registered platform does declare, asked about under a platform id
    // that is not registered: the refusal has to be about the platform, or a
    // missing table row would be reported as a missing route.
    expect(() => coordinationSegmentForRequest('deepseek-web', `https://chat.deepseek.com${DEEPSEEK_LIST_PATH}`))
      .toThrow('no backfill request plan for deepseek-web');
    // The lookup is exact, so neither a different case nor a stray character
    // resolves to the plan it is closest to.
    expect(() => coordinationSegmentForRequest('ChatGPT', `https://chatgpt.com${CHATGPT_LIST_PATH}`))
      .toThrow('no backfill request plan for ChatGPT');
    expect(() => coordinationSegmentForRequest('chatgpt ', `https://chatgpt.com${CHATGPT_LIST_PATH}`))
      .toThrow('no backfill request plan for chatgpt ');
  });

  it('refuses an undeclared query on the path a form plan declares for both segments', () => {
    // The premise, read off the registry rather than assumed. Gemini is the only
    // plan in the table whose list and detail paths are the same string, and the
    // only one declaring a form for either segment, and that conjunction is the
    // whole condition — so it is asserted here, where this file's claim lives.
    expect(GEMINI_PLAN.listPath).toBe(GEMINI_PLAN.detailPath);
    expect(GEMINI_PLAN.listPath).toBe('/_/BardChatUi/data/batchexecute');
    expect(GEMINI_PLAN.listTokenForm).toBeDefined();
    expect(GEMINI_PLAN.detailForm).toBeDefined();

    // Each declared query is classified by the segment that declared it, so the
    // refusal below is only reachable once neither matched.
    const origin = 'https://gemini.google.com';
    expect(coordinationSegmentForRequest('gemini', GEMINI_PLAN.listUrl(origin, 0, 20))).toBe('enumerate');
    expect(coordinationSegmentForRequest('gemini', GEMINI_PLAN.detailUrl!(origin, 'opaque-id'))).toBe('detail');

    const shared = `${origin}${GEMINI_PLAN.listPath}`;
    // An rpcid no segment declares. (w212-coordination.test.ts pins this one URL
    // too; what is pinned here beyond it is the two neighbours below, which is
    // what makes the sentence about the query and not the route a checked fact.)
    expect(() => coordinationSegmentForRequest('gemini', `${shared}?rpcids=unplanned`))
      .toThrow('request query is not declared by the gemini backfill plan');
    // A declared query carrying **one key more**: refused, not accepted. The
    // query is compared as a set, and the page-context wrapper is the only thing
    // allowed to add its own keys, after this check — a URL that already carries
    // them is a request this build did not build.
    expect(() => coordinationSegmentForRequest('gemini', `${shared}?rpcids=MaZiqc&source-path=/app&rt=c&bl=page-token`))
      .toThrow('request query is not declared by the gemini backfill plan');
    // And a declared query missing one of its three keys.
    expect(() => coordinationSegmentForRequest('gemini', `${shared}?rpcids=MaZiqc&source-path=/app`))
      .toThrow('request query is not declared by the gemini backfill plan');
    // The same undeclared query on a path this plan does not declare is a route
    // miss, and the two sentences are distinguishable: neither is a substring of
    // the other, so which arm refused is visible in the message alone.
    expect(() => coordinationSegmentForRequest('gemini', `${shared}/unplanned?rpcids=unplanned`))
      .toThrow('request is not declared by the gemini backfill plan');
  });

  it('refuses a well-formed URL on a registered platform that matches no route', () => {
    // chatgpt declares its body as a directory prefix ('/backend-api/conversation/'),
    // so the same path without an id is a different route, not a shorter one.
    expect(() => coordinationSegmentForRequest('chatgpt', 'https://chatgpt.com/backend-api/conversation'))
      .toThrow('request is not declared by the chatgpt backfill plan');
    // One segment more than a declared detail route: segment count is structural,
    // so the extra segment cannot be swallowed by the `{id}` placeholder.
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/opaque-org/chat_conversations/opaque-id/messages`))
      .toThrow('request is not declared by the claude backfill plan');
    // A route another platform does declare, asked about here. The table is
    // per-platform, so this is a miss under chatgpt rather than a hit.
    expect(() => coordinationSegmentForRequest('chatgpt', `${CLAUDE}/api/organizations/opaque-org/chat_conversations`))
      .toThrow('request is not declared by the chatgpt backfill plan');
  });

  it('reads a placeholder as one non-empty segment and a literal as exactly itself', () => {
    // The pathnames below are CLAUDE_PLAN's own templates written out, so a change
    // to a declared path shows up here as a failure rather than turning this file
    // into a description of a plan that no longer exists.
    expect(CLAUDE_LIST_PATH_TEMPLATE).toBe('/api/organizations/{org}/chat_conversations');
    expect(CLAUDE_DETAIL_PATH_TEMPLATE).toBe('/api/organizations/{org}/chat_conversations/{id}');

    // A placeholder matches ANY one non-empty segment. Classification here is
    // scope-agnostic on purpose: this function has no resolved organization to
    // compare against, and comparing it is the page-side allowlist's job
    // (tab-port.ts's `scopePathMatches`, which refuses a run addressing another
    // organization). What it owes the coordination counters is only "which bucket".
    expect(coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/an-organization-this-run-never-resolved/chat_conversations`))
      .toBe('enumerate');
    expect(coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/an-organization-this-run-never-resolved/chat_conversations/opaque-id`))
      .toBe('detail');

    // An EMPTY segment is not a match. A doubled slash (an empty `{org}`
    // mid-path) and a single trailing slash (an empty `{id}` at the end) each
    // leave the segment count alone, so these two are refused by the
    // non-empty rule itself and not by the count rule — one on the list
    // template's `{org}`, one on the detail template's `{id}`, each with
    // every other segment spelled correctly.
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations//chat_conversations`))
      .toThrow('request is not declared by the claude backfill plan');
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/opaque-org/chat_conversations/`))
      .toThrow('request is not declared by the claude backfill plan');

    // A literal segment is compared character for character: a near miss, a case
    // difference and a suffix are all refused the same way, with no partial
    // match in either direction.
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/opaque-org/chat_conversation`))
      .toThrow('request is not declared by the claude backfill plan');
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/opaque-org/CHAT_CONVERSATIONS`))
      .toThrow('request is not declared by the claude backfill plan');
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}/api/organizations/opaque-org/chat_conversations_v2`))
      .toThrow('request is not declared by the claude backfill plan');
  });

  it('classifies the three routes a scoped plan declares', () => {
    const scopeInPath = CLAUDE_PLAN.scopeInPath;
    // The premise: this is the plan whose routes carry a template, and the only
    // one in the table today, so its own three paths are what the template
    // branches are written for.
    expect(scopeInPath).toBeDefined();
    const templates = scopeInPath!;

    // The plan's own builders — query and all — and then the same two templates
    // with no query at all. A plan that declares no form is decided on the
    // pathname alone, so a URL carrying no query is the same request, not an
    // undeclared one.
    expect(coordinationSegmentForRequest('claude', CLAUDE_PLAN.listUrl!(CLAUDE, 0, 20))).toBe('enumerate');
    expect(coordinationSegmentForRequest('claude', CLAUDE_PLAN.detailUrl!(CLAUDE, 'opaque-id'))).toBe('detail');
    expect(coordinationSegmentForRequest('claude', `${CLAUDE}${templates.listPath.replace('{org}', 'opaque-org')}`))
      .toBe('enumerate');
    expect(coordinationSegmentForRequest('claude', `${CLAUDE}${templates.detailPath.replace('{org}', 'opaque-org').replace('{id}', 'opaque-id')}`))
      .toBe('detail');

    // The resolver carries no conversation data, and this function's vocabulary
    // has no word for it: it is coordinated as an enumeration request, which is
    // what background.ts asks the lease for when it resolves an organization.
    // The comparison is the exact pathname the plan declares, so a trailing slash
    // is a different path and is refused rather than quietly accepted.
    expect(coordinationSegmentForRequest('claude', `${CLAUDE}${templates.resolvePath}`)).toBe('enumerate');
    expect(() => coordinationSegmentForRequest('claude', `${CLAUDE}${templates.resolvePath}/`))
      .toThrow('request is not declared by the claude backfill plan');
  });
});