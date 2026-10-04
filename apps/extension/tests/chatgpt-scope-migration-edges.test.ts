/**
 * W614 · The edges of `migrateChatGptWorkspaceScopes` — the one place that takes a
 * pre-fingerprint ChatGPT workspace id out of durable storage.
 *
 * W299-D covers the happy path: one raw scope, one header, one pending debt,
 * migrated once and idempotent afterwards. This file covers the seven answers that
 * path never asks, and each is a way the migration could turn "there is nothing to
 * do here" into a rewrite it had no licence to make:
 *
 *  1. a `null` store resolves and touches nothing — this runs before any backfill
 *     work, so it must not be where an install's storage is first written;
 *  2. nothing to migrate returns **before** the account salt is read or created, so
 *     an install with no raw scope does not grow a salt because the migration ran;
 *  3. a store that cannot list its keys (a restricted embedder) still migrates the
 *     scopes it can *name* from the target registry — and only the ones that are
 *     not fingerprinted yet;
 *  4. an already-fingerprinted scope is not re-keyed: header not removed, not
 *     rewritten, and its debt set not touched;
 *  5. a target entry that is not an object is not a scope this may read, so it is
 *     carried through verbatim whatever it happens to be;
 *  6. an unreadable account salt is a named refusal, not a reason to invent one: a
 *     fresh salt over an existing one would silently re-key every later bundle;
 *  7. the scrub rewrites the records that repeat a migrated raw scope (the
 *     round-robin cursor names a scope as a map key) and leaves every other record
 *     byte-identical — a write for a record that did not change is a spurious write.
 *
 * Fixtures only: opaque synthetic workspace ids, a synthetic salt, a synthetic
 * cursor. No logged-in session, no real account id, no network.
 */

import { describe, expect, it } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { ACCOUNT_SALT_KEY } from '../lib/account-fingerprint';
import { BACKFILL_CURSOR_KEY, BACKFILL_TARGETS_KEY } from '../lib/backfill/alarm';
import { migrateChatGptWorkspaceScopes } from '../lib/backfill/chatgpt-scope-migration';
import { fingerprintChatGptWorkspace } from '../lib/backfill/chatgpt-workspace';
import { applyDebtDiff, readDebtSet } from '../lib/backfill/debt-store';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { BACKFILL_STATE_VERSION, stateKey } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// Synthetic fixtures. No real account, workspace or conversation id below.
// ---------------------------------------------------------------------------
const ORIGIN = 'https://chatgpt.com';
/** A pre-fingerprint workspace id: 64 opaque characters, never a real one. */
const RAW_A = 'a1b2c3d4e5f60718'.repeat(4);
/** A scope this build has already migrated, written by some earlier run. */
const FINGERPRINTED_B = `chatgpt:fp1:${'b'.repeat(64)}`;
const SCOPE_A = `chatgpt:${RAW_A}`;
/** A key that names nothing about ChatGPT, so the scrub pass has to walk it. */
const UNRELATED_KEY = 'cs_fixture_unrelated_record_v1';
/** A second one that repeats the raw scope as a **value**. */
const HINT_KEY = 'cs_fixture_scope_hint_v1';

/** A header at the shape `isHeader` accepts, so the migration's copy branch runs. */
function headerFixture(platform: string, scope: string): Record<string, unknown> {
  return {
    v: BACKFILL_STATE_VERSION,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-30', fetched: 0, failed: 0 },
    halted: null,
  };
}

/**
 * A syntactically valid stored salt (a 32-byte key, which is what `readSalt`
 * requires), so a test's fingerprint comes out of its own fixture and the
 * migration never *creates* one. That keeps every "what was written" assertion
 * below about the writes the migration actually owns.
 */
function saltFixture(): Record<string, unknown> {
  return { id: 'fixture-salt-id', key: btoa('0123456789abcdef0123456789abcdef'), createdAt: 0 };
}

/**
 * `memoryStore` plus a trace of the port's own calls.
 *
 * The trace is the point of this file's store: several of the properties under
 * test are statements about *reads and writes that did not happen* (the salt is
 * never read on an early return; an untouched record is never written again), and
 * the resulting bytes alone cannot distinguish "nothing happened" from "it
 * happened and undid itself".
 */
function probeStore(
  seed: Record<string, unknown> = {},
  options: { keysReject?: boolean } = {},
) {
  const base = memoryStore(seed);
  const loads: string[] = [];
  const saves: Array<{ key: string; value: unknown }> = [];
  const removes: string[] = [];
  let keysCalls = 0;
  const store: BackfillStore = {
    async load(key: string): Promise<unknown> {
      loads.push(key);
      return base.load(key);
    },
    async save(key: string, value: unknown): Promise<void> {
      saves.push({ key, value });
      await base.save(key, value);
    },
    async remove(key: string): Promise<void> {
      removes.push(key);
      await base.remove(key);
    },
    async keys(): Promise<string[]> {
      keysCalls += 1;
      if (options.keysReject) throw new Error('listing storage.local is refused in this embedder');
      return base.keys();
    },
  };
  return {
    store,
    data: base.data,
    loads,
    saves,
    removes,
    savedKeys: () => saves.map((row) => row.key),
    keysCalls: () => keysCalls,
  };
}

describe('W614 · ChatGPT scope migration · edges', () => {
  it('🔴 a null store resolves, and no database is opened behind it', async () => {
    // A `null` store has no port to trace, so the observable half of "no storage
    // touch" is the debt store: an implementation that ran past the guard would
    // reach `rekeyDebtScope`, and that opens the database.
    class CountingFactory extends IDBFactory {
      readonly opens: string[] = [];
      override open(name: string, version?: number): IDBOpenDBRequest {
        this.opens.push(name);
        return super.open(name, version);
      }
    }
    const factory = new CountingFactory();
    (globalThis as Record<string, unknown>).indexedDB = factory;

    await expect(migrateChatGptWorkspaceScopes(null)).resolves.toBeUndefined();

    expect(factory.opens).toEqual([]);
  });

  it('🔴 nothing to migrate returns before the account salt is read or created', async () => {
    const claudeScope = 'organization-fixture';
    const claudeKey = stateKey('claude', claudeScope);
    const probe = probeStore({
      [claudeKey]: headerFixture('claude', claudeScope),
      [BACKFILL_TARGETS_KEY]: [{ platform: 'claude', origin: 'https://claude.ai', scope: claudeScope, at: 1 }],
    });
    const before = JSON.stringify(probe.data);

    await migrateChatGptWorkspaceScopes(probe.store);

    // A ChatGPT registry row and a ChatGPT header key are both absent here, which
    // is the whole premise: no candidates and no raw targets.
    expect(probe.keysCalls()).toBe(1);
    expect(probe.loads).toContain(BACKFILL_TARGETS_KEY);
    expect(probe.loads).not.toContain(ACCOUNT_SALT_KEY);
    expect(probe.savedKeys()).not.toContain(ACCOUNT_SALT_KEY);
    expect(probe.removes).toEqual([]);
    expect(Object.keys(probe.data)).not.toContain(ACCOUNT_SALT_KEY);
    expect(JSON.stringify(probe.data)).toBe(before);
  });

  it('🔴 a store that cannot list its keys migrates the scopes the registry names, and only the raw ones', async () => {
    const oldKeyA = stateKey('chatgpt', SCOPE_A);
    const fingerprintedKeyB = stateKey('chatgpt', FINGERPRINTED_B);
    const headerB = headerFixture('chatgpt', FINGERPRINTED_B);
    const probe = probeStore({
      [ACCOUNT_SALT_KEY]: saltFixture(),
      [BACKFILL_TARGETS_KEY]: [
        { platform: 'chatgpt', origin: ORIGIN, scope: SCOPE_A, at: 1 },
        { platform: 'chatgpt', origin: ORIGIN, scope: FINGERPRINTED_B, at: 2 },
      ],
      [oldKeyA]: headerFixture('chatgpt', SCOPE_A),
      [fingerprintedKeyB]: headerB,
    }, { keysReject: true });

    const expectedScope = await fingerprintChatGptWorkspace(probe.store, RAW_A);
    if (!expectedScope) throw new Error('the synthetic salt must produce a fingerprint');

    await migrateChatGptWorkspaceScopes(probe.store);

    expect(probe.keysCalls()).toBe(1);
    expect(expectedScope).toMatch(/^chatgpt:fp1:[0-9a-f]{64}$/);
    // Derived from the registry row, because the key listing refused to answer.
    expect(await probe.store.load(oldKeyA)).toBeNull();
    expect(probe.removes).toEqual([oldKeyA]);
    expect(await probe.store.load(stateKey('chatgpt', expectedScope))).toEqual({
      ...headerFixture('chatgpt', SCOPE_A),
      scope: expectedScope,
    });
    // The fingerprinted row is not a candidate at all: untouched header, no re-key.
    expect(await probe.store.load(fingerprintedKeyB)).toEqual(headerB);
    expect(probe.savedKeys()).not.toContain(fingerprintedKeyB);
    expect(probe.removes).not.toContain(fingerprintedKeyB);
    expect((await probe.store.load(BACKFILL_TARGETS_KEY) as Array<{ scope: string }>).map((row) => row.scope))
      .toEqual([expectedScope, FINGERPRINTED_B]);
  });

  it('🔴 an already-fingerprinted scope is skipped: not removed, not rewritten, and its debt set not re-keyed', async () => {
    const oldKeyA = stateKey('chatgpt', SCOPE_A);
    const fingerprintedKeyB = stateKey('chatgpt', FINGERPRINTED_B);
    const headerB = headerFixture('chatgpt', FINGERPRINTED_B);
    const probe = probeStore({
      [ACCOUNT_SALT_KEY]: saltFixture(),
      [BACKFILL_TARGETS_KEY]: [
        { platform: 'chatgpt', origin: ORIGIN, scope: SCOPE_A, at: 1 },
        { platform: 'chatgpt', origin: ORIGIN, scope: FINGERPRINTED_B, at: 2 },
      ],
      [oldKeyA]: headerFixture('chatgpt', SCOPE_A),
      [fingerprintedKeyB]: headerB,
    });
    const expectedScope = await fingerprintChatGptWorkspace(probe.store, RAW_A);
    if (!expectedScope) throw new Error('the synthetic salt must produce a fingerprint');
    // A debt under the fingerprinted scope, so "not re-keyed" is a claim about the
    // debt store and not only about the header at that key. The predicate is
    // guarded in more than one place in the module (the key filter, the registry
    // branch and the migration loop each re-check it), so removing one guard alone
    // is not observable from here — the skip is therefore asserted on every surface
    // it can reach: the header, the debt set and the registry row.
    expect(await applyDebtDiff(
      'chatgpt',
      FINGERPRINTED_B,
      { enqueue: ['debt-under-fingerprinted-scope'], settle: [], drop: [] },
      1,
    )).toBe(true);

    await migrateChatGptWorkspaceScopes(probe.store);

    expect(await probe.store.load(fingerprintedKeyB)).toEqual(headerB);
    expect(probe.savedKeys()).not.toContain(fingerprintedKeyB);
    expect(probe.removes).not.toContain(fingerprintedKeyB);
    expect((await readDebtSet('chatgpt', FINGERPRINTED_B))?.pending).toEqual(['debt-under-fingerprinted-scope']);
    // The raw scope in the same run really was migrated, so the run was not a no-op.
    expect(await probe.store.load(stateKey('chatgpt', expectedScope))).toEqual({
      ...headerFixture('chatgpt', SCOPE_A),
      scope: expectedScope,
    });
  });

  it('🔴 a target entry that is not an object survives the rewritten registry verbatim', async () => {
    const probe = probeStore({
      [ACCOUNT_SALT_KEY]: saltFixture(),
      [BACKFILL_TARGETS_KEY]: [
        null,
        'chatgpt:bare-string-entry-fixture',
        { platform: 'chatgpt', origin: ORIGIN, scope: SCOPE_A, at: 7 },
      ],
    });
    const expectedScope = await fingerprintChatGptWorkspace(probe.store, RAW_A);
    if (!expectedScope) throw new Error('the synthetic salt must produce a fingerprint');

    await migrateChatGptWorkspaceScopes(probe.store);

    const rows = await probe.store.load(BACKFILL_TARGETS_KEY) as unknown[];
    expect(rows).toHaveLength(3);
    expect(rows[0]).toBeNull();
    expect(rows[1]).toBe('chatgpt:bare-string-entry-fixture');
    // The one row this migration does own: the scope is rewritten and the rest of
    // the row — origin and `at` — is carried rather than rebuilt.
    expect(rows[2]).toEqual({ platform: 'chatgpt', origin: ORIGIN, scope: expectedScope, at: 7 });
    // One write, for the registry it rewrote. A non-object entry is not a reason to
    // write anything else.
    expect(probe.savedKeys()).toEqual([BACKFILL_TARGETS_KEY]);
    expect(probe.removes).toEqual([]);
  });

  it('🔴 an unreadable account salt is a named refusal, and storage is left exactly as it was', async () => {
    // Base64 of "short": a well-formed string that is not ACCOUNT_SALT_BYTES.
    const unreadableSalt = { id: 'fixture-salt-id', key: 'c2hvcnQ=', createdAt: 0 };
    const probe = probeStore({
      [ACCOUNT_SALT_KEY]: unreadableSalt,
      [BACKFILL_TARGETS_KEY]: [{ platform: 'chatgpt', origin: ORIGIN, scope: SCOPE_A, at: 1 }],
      [stateKey('chatgpt', SCOPE_A)]: headerFixture('chatgpt', SCOPE_A),
    });
    const before = JSON.stringify(probe.data);

    await expect(migrateChatGptWorkspaceScopes(probe.store)).rejects.toThrow(
      'ChatGPT scope migration requires the existing account fingerprint key',
    );

    // In particular it did not create a salt over the one it could not read.
    expect(probe.saves).toEqual([]);
    expect(probe.removes).toEqual([]);
    expect(JSON.stringify(probe.data)).toBe(before);
    expect(await probe.store.load(ACCOUNT_SALT_KEY)).toEqual(unreadableSalt);
  });

  it('🔴 the scrub rewrites a record that repeats the raw scope, and writes nothing else', async () => {
    const unrelated = { hello: 'world', revision: 3 };
    const probe = probeStore({
      [ACCOUNT_SALT_KEY]: saltFixture(),
      [BACKFILL_TARGETS_KEY]: [{ platform: 'chatgpt', origin: ORIGIN, scope: SCOPE_A, at: 1 }],
      [stateKey('chatgpt', SCOPE_A)]: headerFixture('chatgpt', SCOPE_A),
      // The round-robin cursor stores the served target's identity — platform and
      // scope — as a map key, so the raw scope reaches it as a **key**.
      [BACKFILL_CURSOR_KEY]: { served: { [`chatgpt\0${SCOPE_A}`]: 0 }, revision: 4 },
      [HINT_KEY]: { lastScope: SCOPE_A, attempts: 2 },
      [UNRELATED_KEY]: unrelated,
    });
    const expectedScope = await fingerprintChatGptWorkspace(probe.store, RAW_A);
    if (!expectedScope) throw new Error('the synthetic salt must produce a fingerprint');

    await migrateChatGptWorkspaceScopes(probe.store);

    expect(await probe.store.load(BACKFILL_CURSOR_KEY)).toEqual({
      served: { [`chatgpt\0${expectedScope}`]: 0 },
      revision: 4,
    });
    expect(await probe.store.load(HINT_KEY)).toEqual({ lastScope: expectedScope, attempts: 2 });
    // Byte-identical, and not written again: the record a scrub touched for no
    // reason would show up as a write, not only as changed bytes.
    expect(await probe.store.load(UNRELATED_KEY)).toEqual(unrelated);
    expect(JSON.stringify(probe.data[UNRELATED_KEY])).toBe(JSON.stringify(unrelated));
    expect(probe.savedKeys()).not.toContain(UNRELATED_KEY);
    // The complete list of writes: the migrated header, the registry, and the two
    // records that named the raw scope. Nothing else.
    expect(probe.savedKeys()).toEqual([
      stateKey('chatgpt', expectedScope),
      BACKFILL_TARGETS_KEY,
      BACKFILL_CURSOR_KEY,
      HINT_KEY,
    ]);
    expect(JSON.stringify(probe.data)).not.toContain(RAW_A);
  });
});