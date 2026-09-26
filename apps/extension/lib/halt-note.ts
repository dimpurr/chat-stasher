/**
 * 🔴 W113 · **One stop, in plain words — the single place that mapping happens.**
 *
 * Lifted out of `popup-view.ts` unchanged when the coverage page needed the same sentences. It lives in a
 * module of its own rather than in `popup-view.ts` so that the coverage page can use it without pulling in
 * the popup's whole world (the native-host types, the stage summary, the dashboard opener).
 *
 * It is a function rather than a second copy on purpose: this branch chain is where "which stop is this" is
 * decided, every arm of it exists because an earlier version read one state out as the wrong sentence, and
 * two copies would drift in the direction that is hardest to notice — a page saying "it stopped" about a
 * leg that is waiting out a backoff, or naming a platform-level stop as a change to the API.
 *
 * It takes the four things the sentences interpolate rather than a whole popup model, so a caller that is
 * not the popup does not have to build one. `null` when there is no record to word.
 */

import { t } from './i18n';
import { retryMinutesLeft } from './backfill/progress';
import { haltClassOf, type BackfillState } from './backfill/types';

export function haltNote(
  halted: BackfillState['halted'],
  platform: string,
  pendingCount: number,
  now: number,
): string | null {
  if (!halted) return null;
    // 🔴 C22 · 'unsupported-platform' is not "it broke and stopped", it is "we
    //    have not written this platform yet". Both must leave a trace, and they
    //    must never be the same sentence.
    if (halted.reason === 'unsupported-platform') {
      return t('popup.notes.halted.unsupportedPlatform', {
        platform: platform,
        detail: halted.detail,
      });
    } else if (halted.reason === 'auth-refused') {
      // 🔴 W61 · The platform refused the request **in its own answer**, with HTTP
      //    200 — so no status line this popup prints could have shown it, and the
      //    reason it must never read as is `other`. `other` would say "the platform
      //    refused a request (auth-refused) — <detail>", which describes the record
      //    instead of the two things a user needs: that nothing was read, and that
      //    the fix is a login.
      //
      // 🔴 W61b · It carries the retry moment as well (the reason is transient now,
      //    `haltClassOf`), and both halves have to be in one sentence: the login is
      //    what makes the *next* round work, and the leg is what comes back to ask.
      //    Leaving the retry out would describe a stop the record does not describe
      //    — and, before the fix, a stop nothing in the product could clear.
      //
      // 🔴 W61b · And the sentence says nothing about *which* request was refused.
      //    The same reason is raised on the list segment and on the body segment,
      //    and the first version of it named the list: for a body refusal that
      //    sentence was false in both directions (the list had been read, and its
      //    ids were already pending — the review's third finding). The sentence a
      //    user sees must be true of every state it is printed in; `{detail}` names
      //    the segment for anyone who needs it.
      return t('popup.notes.halted.authRefused', {
        platform: platform,
        detail: halted.detail,
        attempts: halted.attempts ?? 1,
        minutes: retryMinutesLeft(halted, now),
      });
    } else if (halted.reason === 'refused-unknown') {
      // 🔴 W61b · The same in-band refusal, with a code this build cannot read. It
      //    gets its own sentence rather than the generic `waitingRetry` one, and the
      //    difference is the whole reason it exists: `waitingRetry` says the leg is
      //    waiting out a backoff after the platform "refused or dropped a request",
      //    which is a claim about *why* — true for a rate limit, and not established
      //    here. This one says what is true: the platform named a code this build
      //    does not know, the code and its message are in the detail, and the leg
      //    will look again.
      return t('popup.notes.halted.refusedUnknown', {
        platform: platform,
        detail: halted.detail,
        attempts: halted.attempts ?? 1,
        minutes: retryMinutesLeft(halted, now),
      });
    } else if (haltClassOf(halted.reason) === 'transient') {
      // 🔴 W13 · This is the sentence that did not exist, and its absence is why a
      //    real account sat at 0 archived for over an hour. A transient stop must
      //    NOT read like the `other` fallback below ("this leg has stopped"): it has
      //    not stopped. It is waiting out a backoff, it will come back by itself,
      //    and not one debt was written off while it waited. So the note says all
      //    four of those things, plus the one number the user actually wants —
      //    when the next attempt is.
      return t('popup.notes.halted.waitingRetry', {
        reason: halted.reason,
        attempts: halted.attempts ?? 1,
        minutes: retryMinutesLeft(halted, now),
        detail: halted.detail,
      });
    } else if (halted.reason === 'org-ambiguous' || halted.reason === 'org-unresolved') {
      // 🔴 W31c · The two organization halts. Each gets its own sentence, and
      //    neither may fall through to `other`: `other` prints the reason code and
      //    the technical detail, which is a description of the state rather than
      //    the one thing a user can do about it. Both of these have exactly one
      //    action, and it is a human action — which is why the leg stopped.
      return t(halted.reason === 'org-ambiguous'
        ? 'popup.notes.halted.orgAmbiguous'
        : 'popup.notes.halted.orgUnresolved');
    } else if (halted.reason === 'account-changed') {
      // 🔴 W199 · **The one stop whose remedy is "use that account again", and the only one
      //    whose record is a suspension rather than only a halt.**
      //
      //    It must not fall through to `waitingRetry`: that sentence says the leg is waiting
      //    out a backoff and will come back by itself, and this stop is deliberately built so
      //    that it will not — coming back means sending the same request under the same wrong
      //    account. It must not fall through to `other` either: `other` prints the reason code
      //    and the technical detail, which describes the record instead of the one thing the
      //    user can do about it.
      //
      //    What the sentence has to carry, and why each half is there: **the account this
      //    scope belongs to is not the account signed in** (the cause); **nothing was lost
      //    and nothing is owed here was written off** (the fear a user has about a stopped
      //    archive); and **it resumes when that account is used again** (the action). The
      //    detail is appended for whoever reads the record, and it names the two segments'
      //    comparison without either id.
      return t('popup.notes.halted.accountChanged', { detail: halted.detail });
    } else if (halted.reason === 'detail-unsupported') {
      // 🔴 C26 · This one must **not** say "stopped before issuing any request" —
      //    the list request really went out and conversations really were listed.
      //    Stopping half way and never starting are two different things to a user.
      return t('popup.notes.halted.detailUnsupported', {
        platform: platform,
        pending: pendingCount,
        detail: halted.detail,
      });
    } else {
      return t('popup.notes.halted.other', {
        reason: halted.reason,
        detail: halted.detail,
      });
    }
  return null;
}


