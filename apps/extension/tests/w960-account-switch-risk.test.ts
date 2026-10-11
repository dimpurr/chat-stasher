/**
 * W960 · **The account-switch gap is visible on the two rows that hid it — and
 * on no row that does not need it.**
 *
 * ## The gap this pins, in the words it was found in
 *
 * W128 (public issue #4) names the failure: inside one browser profile, a
 * switch from account A to account B can put B's list ids into A's run scope
 * and let a same-id B body settle as A — **a mis-attribution, not a deletion**.
 * The W126 audit marks every web platform RISK for it, and W944's inventory
 * (§E) tightened which rows that is true for: the only account guards that
 * actually FIRE today are ChatGPT's `ChatGPT-Account-Id` header lease (W299 /
 * W303) and Claude's organization check backed by a person fingerprint (W239 /
 * W337) — the leases `ACCOUNT_LEASE_PLATFORMS` declares for deepseek,
 * perplexity, gemini and grok are real declarations over wires that carry no
 * verified account id, so they never fire (a `'default'` scope is `unleased`).
 *
 * ## Why the table needed a SECOND caveat
 *
 * Every row owns exactly one `knownIssue` slot, and W944 §H item 4 found the
 * two rows whose slot was already spent on a different issue:
 *  · `deepseek`, `perplexity`, `kimi` spend theirs ON the account gap — their
 *    rows named it already;
 *  · `chatgpt` and `claude` spend theirs on the residual their firing guard
 *    leaves (a workspace / same-organization switch);
 *  · `gemini` (slot spent on the 20-page detail refusal) and `grok` (slot spent
 *    on the un-paged body endpoint) had nothing left, so the account gap was
 *    **invisible from the platform table** while the W126 audit marks both
 *    RISK.
 *
 * W960 adds `accountSwitchRisk`: a second, one-line, editorial caveat on the
 * row. This file pins the discipline that keeps it a scalpel and never a
 * blanket flag:
 *
 *  1. 🔴 the two rows that spent their slot elsewhere now report the risk, in
 *     the same sentence the always-unguarded rows use — it is the same failure;
 *  2. 🔴 every `knownIssue` slot keeps exactly the text it held before — the
 *     slot is not displaced, widened, or "improved";
 *  3. 🔴 the second caveat never duplicates a slot that already names the gap,
 *     and never marks a row whose guard fires — a genuinely guarded platform
 *     is not marked risky;
 *  4. 🔴 the field records the RISK, not a guard or a fingerprint: the value
 *     says "not yet guarded" and points at issue #4, nothing more.
 */

import { describe, expect, it } from 'vitest';
import { ALL_PLATFORMS, type ChatPlatform } from '../lib/contract';
import { ORGANIZATION_SCOPED_PLATFORMS } from '../lib/backfill/enumerate';

/**
 * The sentence this table already uses for issue #4 on an unguarded row. The
 * two rows that could not say it in `knownIssue` say the SAME sentence here —
 * one fact, one wording, so a reader (or a grep) finds every row that carries
 * the gap, whichever slot it lives in.
 */
const UNGUARDED_ACCOUNT_SWITCH =
  'an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4)';

function row(id: string): ChatPlatform {
  const found = ALL_PLATFORMS.find((r) => r.id === id);
  if (!found) throw new Error(`no platform row for '${id}'`);
  return found;
}

describe('W960 · the account-switch risk beside the spent knownIssue slot', () => {
  it('🔴 gemini and grok report the account-switch risk in the second caveat', () => {
    for (const id of ['gemini', 'grok']) {
      expect(row(id).accountSwitchRisk, id).toBe(UNGUARDED_ACCOUNT_SWITCH);
      // The sentence records the risk and points where it is tracked; it never
      // claims a guard or invents a fingerprint for a wire nobody measured.
      expect(row(id).accountSwitchRisk, id).toMatch(/\(see issue #4\)$/);
    }
  });

  it('🔴 every knownIssue slot keeps exactly the text it held before W960', () => {
    // Pinned verbatim, per row: the single slot was not displaced by the second
    // caveat — it keeps saying what it said on the day W944 found the gap.
    const before: Record<string, string> = {
      deepseek: 'an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4)',
      perplexity: 'an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4)',
      chatgpt: 'a workspace switch is not yet pinned to the run scope: the new workspace list ids can land in the old scope (see issue #4)',
      gemini: 'a conversation needing more than 20 detail pages is refused and not archived in part (see docs-dev/privacy.md)',
      claude: 'two accounts inside one organization are not yet distinguished by the organization check (see issue #4)',
      kimi: 'an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4)',
      grok: 'the body endpoint is not paged; whether a long conversation comes back complete is unverified (see docs-dev/threat-model.md)',
    };
    expect(ALL_PLATFORMS.map((r) => r.id).sort()).toEqual(Object.keys(before).sort());
    for (const r of ALL_PLATFORMS) {
      expect(r.knownIssue, r.id).toBe(before[r.id]);
    }
  });

  it('🔴 the second caveat marks only the two rows whose slot was spent elsewhere', () => {
    // Asserted by id, not derived: the field is exactly where W944 §H item 4
    // found the invisible gap, and nowhere else. A row added or blanket-flagged
    // in the future has to be seen here.
    expect(ALL_PLATFORMS.map((r) => r.id).sort()).toEqual(
      ['chatgpt', 'claude', 'deepseek', 'gemini', 'grok', 'kimi', 'perplexity'].sort(),
    );
    expect(ALL_PLATFORMS.filter((r) => r.accountSwitchRisk !== undefined).map((r) => r.id).sort())
      .toEqual(['gemini', 'grok']);
  });

  it('🔴 a genuinely guarded platform is not marked risky', () => {
    // W944 §E: the only two account guards that FIRE today.
    //  · chatgpt — W303's run lease over the request's own
    //    `ChatGPT-Account-Id` header (AccountIdSource
    //    'request-header-chatgpt-account-id'): a switch that changes the header
    //    halts the run as account-changed. Its knownIssue names the residual
    //    (a switch inside one workspace value), which is the honest state of a
    //    platform whose guard fires.
    expect(row('chatgpt').accountSwitchRisk).toBeUndefined();
    //  · claude — the organization guard below halts a cross-organization
    //    switch before a request is built, and W337's person fingerprint
    //    (claude-account.ts) supplies the person axis. Its knownIssue names the
    //    residual (two accounts inside one organization).
    expect(ORGANIZATION_SCOPED_PLATFORMS).toContain('claude');
    expect(row('claude').accountSwitchRisk).toBeUndefined();
  });

  it('🔴 the second caveat never duplicates a slot that already names the gap', () => {
    // One fact, one place: a carrier's knownIssue is a DIFFERENT issue; a row
    // whose knownIssue names the account gap (or issue #4) has no second
    // caveat — that is what "the rows already naming it are unchanged" means.
    for (const r of ALL_PLATFORMS) {
      if (r.accountSwitchRisk === undefined) continue;
      expect(r.knownIssue, r.id).toBeDefined();
      expect(r.knownIssue ?? '', r.id).not.toMatch(/account switch|issue #4/);
    }
    for (const id of ['deepseek', 'perplexity', 'kimi', 'chatgpt', 'claude']) {
      expect(row(id).accountSwitchRisk, `${id} already names the gap in its own slot`).toBeUndefined();
    }
  });
});
