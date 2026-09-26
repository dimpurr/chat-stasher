/**
 * 🔴 W199 · W128 step 2 — **the run identity lease.**
 *
 * ## The problem this exists for
 *
 * One browser profile can be signed in to account B after having been signed in to
 * account A. Step 1 (W165) made that *visible*: every captured bundle carries an
 * irreversible fingerprint of the account it came from. It changed no behaviour —
 * its own report says so in as many words ("**Nothing enforces anything yet.** The
 * fingerprint is recorded, not compared.").
 *
 * This module is the comparison. Three consequences of a switch are addressed, and
 * they are addressed in two different places because they are two different events:
 *
 *  1. **A run for account A keeps going while B is signed in.** Its list request is
 *     answered by B, so B's conversation ids would be enqueued into A's ledger; its
 *     detail requests are answered by B, so a body that is not A's could be archived
 *     as A's. This is the *run* half, and it is decided against the lease a scope
 *     carries (`compareAccountLease`, consulted by the engine at each attribution
 *     point).
 *  2. **A's scope stays registered after the user has moved to B**, so the alarm
 *     keeps waking it. This is the *registry* half: an observation of B suspends
 *     every other scope of that platform whose lease says something else
 *     (`suspensionFor`), and the suspension is what stops the next alarm wake —
 *     `scopeRetryDue` consults it, and it does not expire on a clock.
 *  3. **A suspension that nothing can lift** would trade "wrong data" for "no data",
 *     which is not a trade this project makes. So a suspension is lifted by an
 *     observation that **agrees** with the suspended scope's lease — in practice the
 *     user signing back in and using that account, whose very next capture is that
 *     observation (`agreesWithLease`).
 *
 * ## What makes the comparison meaningful, stated as an intent rather than a list
 *
 *  · **A lease is the same value step 1 puts on a bundle, for the same account.**
 *    Both are `fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, platform, id)`
 *    over the same id — for a path-unscoped plan, the id the scope key is built from
 *    (ADR-002's account axis) — so a lease and an observed bundle fingerprint are
 *    comparable *by construction* and not by convention. One construction, two
 *    readers.
 *  · **Two values may only be compared when their `saltId`s are equal.** A cleared
 *    `cs_account_salt_v1`, a reinstall or a second profile produce values that are
 *    not about each other, and a comparison that ignored this would accuse an
 *    unchanged account of switching (step 1's own rule: a repair must never invent a
 *    switch).
 *  · **An absence is never an accusation.** A scope with no lease, and a response
 *    with no account visible in it, produce `unleased` / `incomparable` — the leg
 *    carries on exactly as it did before this module existed, and says on the record
 *    that it could not tell. Only a **proven** disagreement stops work.
 *
 * ## What this module deliberately does not do
 *
 *  · **It does not read an email or a handle, and it never sees one**: the id it
 *    fingerprints is the scope string, which the ADR-002 scan already refused an
 *    email or a handle for (`accountIdFromCapture`, lib/account-fingerprint.ts).
 *  · **It holds no ids.** Both halves of every comparison are digests; the raw id
 *    exists only inside `fingerprintAccountId`'s message and is not returned.
 *  · **It never issues a request, reads a page or writes storage.** Every function
 *    here is pure but for the salt read, and the salt read goes through step 1's own
 *    `loadOrCreateAccountSalt`.
 */

import {
  ACCOUNT_FINGERPRINT_DOMAIN,
  fingerprintAccountId,
  loadOrCreateAccountSalt,
} from '../account-fingerprint';
import type { AccountFingerprint, AccountIdSource } from '../contract';
import { backfillPlanFor, planHoldsAccountLease } from './enumerate';
import type { BackfillStore } from './store';
import type { AccountIdentity, AccountLease, AccountSuspension } from './types';

/**
 * Why a scope has no lease this install can read.
 *
 * Every value is a different fact and none of them is "the account is unknown" in
 * general — which is why they are separate rather than one `unknown`:
 *  · `platform-not-scoped`  — this platform's plan does not declare that its scope
 *    names an account, so there is nothing here to lease at all. Not a failure.
 *  · `scope-names-no-account` — the plan does declare one, and this scope is the
 *    `'default'` sentinel (ADR-002's "the scan found nothing"): a scope whose own
 *    name is "we could not tell".
 *  · `salt-unavailable`     — no store, or a store that could not be written to, so
 *    no salt exists and no value could be reproduced after a reload.
 *  · `crypto-unavailable`   — no usable WebCrypto HMAC in this context.
 */
export type AccountLeaseUnknown =
  | 'platform-not-scoped'
  | 'scope-names-no-account'
  | 'salt-unavailable'
  | 'crypto-unavailable';

export type AccountLeaseReading =
  | { kind: 'lease'; lease: AccountLease }
  | { kind: 'unleased'; reason: AccountLeaseUnknown };

/** The sentinel `backfillTargetFor` writes when the account could not be told. */
const UNRESOLVED_ACCOUNT = 'default';

/**
 * Which of step 1's two mechanisms a plan's account id comes from.
 *
 * Reused rather than re-decided: a plan whose paths carry the scope reads its id out
 * of the page's own request (`request-url-organization`); every other plan's scope is
 * the ADR-002 body axis (`response-body-platform-uid`). Those are exactly the two
 * labels `AccountIdSource` has, and the mapping lives here so a third mechanism would
 * have to be added in one place.
 */
export function leaseSourceFor(platform: string): AccountIdSource {
  return backfillPlanFor(platform)?.scopeInPath ? 'request-url-organization' : 'response-body-platform-uid';
}

/**
 * Whether this platform's run holds an account lease.
 *
 * 🔴 Gated on an explicit declaration, not on one here: the table lives beside `PLANS` in
 *    `./enumerate.ts` (`ACCOUNT_LEASE_PLATFORMS`), where a plan's own body is, so the
 *    boundary is read next to the plans it is about. Re-exported from this module so
 *    every caller of the lease reads one surface.
 */
export { planHoldsAccountLease };

/**
 * The lease for one scope: the fingerprint of **the account the scope names**.
 *
 * This is the "take the account fingerprint at the start of each backfill run" half
 * of step 2, and it needs no request: for a path-unscoped plan the scope key *is* the
 * account id the ADR-002 scan read (`backfillTargetFor`, entrypoints/background.ts),
 * so the value is available before anything is sent. Hashing it rather than storing
 * it raw reuses step 1's construction exactly — same domain, same platform in the
 * message, same per-install salt — so this value and a bundle's `account.value` for
 * the same account are equal and directly comparable.
 *
 * 🔴 It fingerprints the scope **even when the scope came off a record this build did
 *    not write**, and that is deliberate rather than lax: the scope string is not a
 *    guess about an account, it is the address this scope's work is already filed
 *    under, and a fingerprint of the address is the only thing that can disagree with
 *    a fingerprint of an observation. The two ways a scope can fail to name an account
 *    at all are checked above (`'default'`, and a plan that does not declare one).
 */
export async function accountLeaseForScope(
  platform: string,
  scope: string,
  store: BackfillStore | null,
  now: number,
): Promise<AccountLeaseReading> {
  if (!planHoldsAccountLease(platform)) return { kind: 'unleased', reason: 'platform-not-scoped' };
  if (scope.length === 0 || scope === UNRESOLVED_ACCOUNT) {
    return { kind: 'unleased', reason: 'scope-names-no-account' };
  }
  const salt = await loadOrCreateAccountSalt(store);
  if (salt === 'unreadable') {
    // 🔴 A salt record that does not parse is **not** replaced by a fresh one — step
    //    1's rule, and it is the same trap here: a fresh salt would re-key this lease
    //    while the old bundles kept the old value, and the next run would read one
    //    unchanged account as two. So this reports a named unknown and the human's
    //    own remedy (delete the key) stays the only way to start over.
    return { kind: 'unleased', reason: 'salt-unavailable' };
  }
  if (salt === null) return { kind: 'unleased', reason: 'salt-unavailable' };
  const value = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, platform, scope);
  if (value === null) return { kind: 'unleased', reason: 'crypto-unavailable' };
  return {
    kind: 'lease',
    lease: { value, saltId: salt.id, source: leaseSourceFor(platform), at: now },
  };
}

/** The comparable half of a step-1 fingerprint, or null when the capture produced no account. */
export function identityOf(fingerprint: AccountFingerprint): AccountIdentity | null {
  return fingerprint.kind === 'fingerprint'
    ? { value: fingerprint.value, saltId: fingerprint.saltId, source: fingerprint.source }
    : null;
}

/**
 * The comparison, and the whole of it.
 *
 * `'agrees'` is the only outcome that lets a run keep fetching; `'differs'` is the
 * only one that stops it. `'incomparable'` is a value, not a failure: it is what a
 * comparison between two values that are not about each other looks like, and what
 * "there is nothing on one side" looks like.
 *
 * 🔴 `'incomparable'` must never be folded into `'differs'`. Folding it would mean a
 *    cleared salt, a reinstall or a platform whose bodies carry no account read as an
 *    account switch — the archive would be told a lie by the very mechanism added to
 *    stop it lying, and the lie would be unfalsifiable (the append-only archive cannot
 *    take it back).
 */
export type AccountLeaseVerdict = 'agrees' | 'differs' | 'incomparable';

export function compareAccountLease(
  lease: AccountIdentity | null | undefined,
  observed: AccountIdentity | null | undefined,
): AccountLeaseVerdict {
  if (!lease || !observed) return 'incomparable';
  if (lease.saltId !== observed.saltId) return 'incomparable';
  return lease.value === observed.value ? 'agrees' : 'differs';
}

/**
 * The suspension a scope with this lease should carry, when an observation names a
 * **different** account on the same salt.
 *
 * Returns `null` for every case that is not a proven disagreement, and that is the
 * point: this is the only function in the file that accuses anything, and it can only
 * be reached with two comparable values that differ.
 */
export function suspensionFor(
  lease: AccountIdentity | undefined,
  observed: AccountIdentity,
  now: number,
): AccountSuspension | null {
  if (compareAccountLease(lease, observed) !== 'differs') return null;
  return {
    at: now,
    reason: 'account-changed',
    ...(lease ? { lease } : {}),
    observed,
  };
}

/** Does this observation agree with the scope's lease? The lift condition, spelled once. */
export function agreesWithLease(
  lease: AccountIdentity | null | undefined,
  observed: AccountIdentity | null | undefined,
): boolean {
  return compareAccountLease(lease, observed) === 'agrees';
}

/**
 * 🔴 W199 · **What a run may claim before it fetches anything.**
 *
 *  · `'run'` — the run speaks for a recorded account. `take` says whether the lease
 *    was (re)taken by *this* decision and therefore has to be written onto the
 *    header: true when nothing was recorded, and true when what was recorded is
 *    **incomparable** (a different salt). Both are "we had nothing usable", and
 *    neither is a switch.
 *  · `'unleased'` — this platform or this scope names no account, so there is nothing
 *    to claim and nothing to disagree with. Byte-identical to the pre-W199 behaviour.
 *  · `'refuse'` — the recorded lease and the scope's own address name **different
 *    accounts on the same salt**. That is not a switch between a run and a response;
 *    it is the scope key itself disagreeing with what is filed under it, and the run
 *    must not start: one key cannot be two accounts. `detail` says so in words the
 *    popup can print (it carries no id — only the two digests' relationship).
 *
 * 🔴 **Honest note on the refusal branch**, so it is not read as more than it is: this
 *    build cannot produce such a record — the lease is derived from the scope key
 *    itself, two derivations of one input cannot differ, and the two session-id
 *    resolutions on the two paths are the same call (`resolveSessionId` delegates to
 *    `extractSessionId`, which is what `backfillTargetFor` uses). So in production this
 *    arm fires only on a record this build did not write. It is kept because it is the
 *    same guard `lib/coverage-read.ts`'s `isReadableHeaderAt` already applies for the
 *    same reason ("a record whose two halves disagree is not one to show a user as
 *    their progress"), and because obeying such a record — or silently overwriting it —
 *    would both be worse than saying so.
 *
 * 🔴 It is a decision here rather than a halt written inside the engine, so the two
 *    readers of this rule — the run and the coverage page — cannot disagree about what
 *    a scope's state means.
 */
export type RunLease =
  | { kind: 'run'; lease: AccountLease; take: boolean }
  | { kind: 'unleased'; reason: AccountLeaseUnknown }
  | { kind: 'refuse'; detail: string };

export function decideRunLease(
  recorded: AccountLease | undefined,
  reading: AccountLeaseReading,
): RunLease {
  if (reading.kind === 'unleased') return { kind: 'unleased', reason: reading.reason };
  if (!recorded) return { kind: 'run', lease: reading.lease, take: true };
  if (recorded.saltId !== reading.lease.saltId) {
    // 🔴 Incomparable, so re-take. A fingerprint is only meaningful against the salt
    //    it was made with (step 1), and the scope's own address is the authority on
    //    which account this work belongs to, so the incomparable record is replaced
    //    rather than trusted or accused.
    return { kind: 'run', lease: reading.lease, take: true };
  }
  if (recorded.value !== reading.lease.value) {
    return {
      kind: 'refuse',
      detail: 'this scope key and the account fingerprint recorded on it name different'
        + ' accounts for the same install; refusing to run until the record and the scope agree'
        + ' (no conversation was fetched and no debt was changed)',
    };
  }
  return { kind: 'run', lease: recorded, take: false };
}
