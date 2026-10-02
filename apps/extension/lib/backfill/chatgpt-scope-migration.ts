import { ACCOUNT_FINGERPRINT_DOMAIN, fingerprintAccountId, loadOrCreateAccountSalt } from '../account-fingerprint';
import { rekeyDebtScope } from './debt-store';
import { BACKFILL_TARGETS_KEY } from './alarm';
import type { BackfillStore } from './store';
import { BACKFILL_STATE_VERSION, LEGACY_STATE_VERSION, isHeader, isLegacyState, legacyStateKey, stateKey } from './types';

const CURRENT_PREFIX = `cs_backfill_v${BACKFILL_STATE_VERSION}:chatgpt:`;
const LEGACY_PREFIX = `cs_backfill_v${LEGACY_STATE_VERSION}:chatgpt:`;

function scrubKnownRaw(value: unknown, fingerprints: ReadonlyMap<string, string>): unknown {
  if (typeof value === 'string') {
    let next = value;
    for (const [raw, fingerprint] of fingerprints) next = next.split(raw).join(fingerprint);
    return next;
  }
  if (Array.isArray(value)) return value.map((item) => scrubKnownRaw(item, fingerprints));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => {
      let safeKey = key;
      for (const [raw, fingerprint] of fingerprints) safeKey = safeKey.split(raw).join(fingerprint);
      return [safeKey, scrubKnownRaw(item, fingerprints)];
    }));
  }
  return value;
}

/** Remove pre-fingerprint ChatGPT workspace ids from durable storage before any backfill work. */
export async function migrateChatGptWorkspaceScopes(store: BackfillStore | null): Promise<void> {
  if (!store) return;
  const targetsRaw = await store.load(BACKFILL_TARGETS_KEY);
  let candidates: string[] = [];
  let allKeys: string[] | null = null;
  try {
    allKeys = await store.keys();
    candidates = allKeys.filter((key) => {
      const prefix = key.startsWith(CURRENT_PREFIX) ? CURRENT_PREFIX
        : key.startsWith(LEGACY_PREFIX) ? LEGACY_PREFIX : null;
      // Actual ChatGPT workspace scopes include the explicit `chatgpt:` value
      // prefix. Other synthetic or legacy default scopes must keep their meaning.
      return prefix !== null && key.slice(prefix.length).startsWith('chatgpt:');
    });
  } catch {
    // Some embedders expose key reads but not storage.local.get(null). In that
    // case migrate registered scopes, whose exact state keys remain enumerable.
    if (Array.isArray(targetsRaw)) {
      candidates = targetsRaw.flatMap((item) => {
        if (!item || typeof item !== 'object' || (item as { platform?: unknown }).platform !== 'chatgpt'
          || typeof (item as { scope?: unknown }).scope !== 'string') return [];
        const scope = (item as { scope: string }).scope;
        if (!scope.startsWith('chatgpt:') || scope.includes('!workspace-') || scope === 'chatgpt:default'
          || /^[a-f0-9]{64}$/.test(scope.slice('chatgpt:'.length))) return [];
        return [stateKey('chatgpt', scope)];
      });
    }
  }
  const hasRawTargets = Array.isArray(targetsRaw) && targetsRaw.some((item) => item && typeof item === 'object'
    && (item as { platform?: unknown }).platform === 'chatgpt' && typeof (item as { scope?: unknown }).scope === 'string'
    && ((item as { scope: string }).scope.startsWith('chatgpt:'))
    && !((item as { scope: string }).scope.includes('!workspace-'))
    && !((item as { scope: string }).scope === 'chatgpt:default')
    && !(/^[a-f0-9]{64}$/.test((item as { scope: string }).scope.slice('chatgpt:'.length))));
  if (candidates.length === 0 && !hasRawTargets) return;
  const salt = await loadOrCreateAccountSalt(store);
  if (!salt || salt === 'unreadable') throw new Error('ChatGPT scope migration requires the existing account fingerprint key');
  const fingerprints = new Map<string, string>();

  for (const oldKey of candidates) {
    const prefix = oldKey.startsWith(CURRENT_PREFIX) ? CURRENT_PREFIX : LEGACY_PREFIX;
    const oldValue = oldKey.slice(prefix.length);
    if (!oldValue.startsWith('chatgpt:')) continue;
    const oldScope = oldValue;
    if (/^[a-f0-9]{64}$/.test(oldScope.slice('chatgpt:'.length))) continue;
    if (oldScope.includes('!workspace-') || oldScope === 'chatgpt:default') continue;
    const fingerprint = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'chatgpt', oldScope.slice('chatgpt:'.length));
    if (!fingerprint) throw new Error('ChatGPT scope migration could not fingerprint an existing scope');
    fingerprints.set(oldScope.slice('chatgpt:'.length), fingerprint);
    const nextScope = `chatgpt:${fingerprint}`;

    if (!(await rekeyDebtScope('chatgpt', oldScope, nextScope))) {
      throw new Error('ChatGPT pending backfill ids could not be migrated');
    }
    const raw = await store.load(oldKey);
    const nextKey = prefix === CURRENT_PREFIX
      ? stateKey('chatgpt', nextScope)
      : legacyStateKey('chatgpt', nextScope);
    if (prefix === CURRENT_PREFIX && isHeader(raw)) {
      const destination = await store.load(nextKey);
      const value = isHeader(destination) ? destination : { ...raw, scope: nextScope };
      await store.save(nextKey, value);
    } else if (prefix === LEGACY_PREFIX && isLegacyState(raw)) {
      await store.save(nextKey, { ...raw, scope: nextScope });
    }
    await store.remove(oldKey);
  }

  const targets = targetsRaw;
  if (Array.isArray(targets)) {
    const migrated = [];
    for (const item of targets) {
      if (!item || typeof item !== 'object' || (item as { platform?: unknown }).platform !== 'chatgpt'
        || typeof (item as { scope?: unknown }).scope !== 'string') {
        migrated.push(item);
        continue;
      }
      const target = item as { scope: string; [key: string]: unknown };
      if (!target.scope.startsWith('chatgpt:') || target.scope.includes('!workspace-') || target.scope === 'chatgpt:default'
        || /^[a-f0-9]{64}$/.test(target.scope.slice('chatgpt:'.length))) {
        migrated.push(item);
        continue;
      }
      const digest = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'chatgpt', target.scope.slice('chatgpt:'.length));
      if (!digest) throw new Error('ChatGPT target migration could not fingerprint an existing scope');
      fingerprints.set(target.scope.slice('chatgpt:'.length), digest);
      migrated.push({ ...target, scope: `chatgpt:${digest}` });
    }
    await store.save(BACKFILL_TARGETS_KEY, migrated);
  }

  // Other small local records can repeat a registered scope as a map key (the
  // round-robin cursor does). Scrub those known values too, while leaving the
  // outbox and conversation payloads in their separate IndexedDB database alone.
  if (allKeys && fingerprints.size > 0) {
    for (const key of allKeys) {
      if (key === BACKFILL_TARGETS_KEY || key.startsWith(CURRENT_PREFIX) || key.startsWith(LEGACY_PREFIX)) continue;
      const value = await store.load(key);
      const scrubbed = scrubKnownRaw(value, fingerprints);
      if (JSON.stringify(value) !== JSON.stringify(scrubbed)) await store.save(key, scrubbed);
    }
  }
}
