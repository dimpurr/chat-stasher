import type { AccountIdentity } from './types';

/** Opaque, per-install workspace fingerprint. It is safe to retain after the worker boundary. */
declare const chatGptWorkspaceFingerprintBrand: unique symbol;
export type FingerprintedChatGptWorkspace = string & { readonly [chatGptWorkspaceFingerprintBrand]: true };
declare const chatGptIdentityFingerprintBrand: unique symbol;
export type FingerprintedChatGptIdentity = AccountIdentity & { readonly [chatGptIdentityFingerprintBrand]: true };

/** Workspace identity observed from this page's own outgoing ChatGPT requests. */
export type ChatGptWorkspaceResolution =
  | { ok: true; workspace: FingerprintedChatGptWorkspace; identity: AccountIdentity; observed: true }
  | { ok: false; reason: 'workspace-ambiguous' | 'workspace-unresolved'; observed: boolean };

export interface ChatGptWorkspaceObservation { identities: AccountIdentity[] }

/** Canonical storage and comparison form for a ChatGPT workspace identity. */
export function chatGptWorkspaceScope(workspace: unknown): string | null {
  const value = typeof workspace === 'string' ? workspace.trim() : '';
  if (!value || value.length > 512) return null;
  return isFingerprintedChatGptScope(value) ? value : `chatgpt:${value}`;
}

export function observeChatGptWorkspaceFingerprint(
  observation: ChatGptWorkspaceObservation,
  identity: AccountIdentity | null | undefined,
): ChatGptWorkspaceObservation {
  if (!identity || observation.identities.some((item) => item.value === identity.value) || observation.identities.length >= 2) return observation;
  return { identities: [...observation.identities, identity] };
}

export function resolveChatGptWorkspace(observation: ChatGptWorkspaceObservation): ChatGptWorkspaceResolution {
  if (observation.identities.length > 1) return { ok: false, reason: 'workspace-ambiguous', observed: true };
  const identity = observation.identities[0];
  return identity
    ? { ok: true, workspace: fingerprintedChatGptWorkspace(identity.value), identity, observed: true }
    : { ok: false, reason: 'workspace-unresolved', observed: false };
}

function headerValue(headers: unknown, wanted: string): string | null {
  if (typeof Headers !== 'undefined' && headers instanceof Headers) return headers.get(wanted);
  if (Array.isArray(headers)) {
    for (const row of headers) {
      if (Array.isArray(row) && row.length >= 2 && typeof row[0] === 'string'
        && row[0].toLowerCase() === wanted.toLowerCase()) return typeof row[1] === 'string' ? row[1] : null;
    }
    return null;
  }
  if (!headers || typeof headers !== 'object') return null;
  for (const [name, value] of Object.entries(headers)) {
    if (name.toLowerCase() === wanted.toLowerCase() && typeof value === 'string') return value;
  }
  return null;
}

/** A defined init.headers replaces Request headers; otherwise inherit them. */
export function chatGptAccountIdFromRequest(input: unknown, init: unknown): string | null {
  const initHeaders = init && typeof init === 'object' ? (init as { headers?: unknown }).headers : undefined;
  const raw = initHeaders !== undefined
    ? headerValue(initHeaders, 'ChatGPT-Account-Id')
    : input && typeof input === 'object'
      ? headerValue((input as { headers?: unknown }).headers, 'ChatGPT-Account-Id')
      : null;
  const value = raw?.trim();
  return value && value.length <= 512 ? value : null;
}

/** Stable, installation-local storage/coordination scope for a workspace observation. */
export async function fingerprintChatGptWorkspace(
  store: import('./store').BackfillStore | null,
  workspace: unknown,
): Promise<string | null> {
  const value = typeof workspace === 'string' ? workspace.trim() : '';
  if (!value || value.length > 512 || !store) return null;
  const { loadOrCreateAccountSalt, fingerprintAccountId, ACCOUNT_FINGERPRINT_DOMAIN } =
    await import('../account-fingerprint');
  const salt = await loadOrCreateAccountSalt(store);
  if (!salt || salt === 'unreadable') return null;
  const fingerprint = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'chatgpt', value);
  return fingerprint ? fingerprintedChatGptScope(fingerprint) : null;
}

/** Explicit version marker; scope contents are never classified by digest shape alone. */
export function isFingerprintedChatGptScope(scope: string): boolean {
  return /^chatgpt:fp1:[0-9a-f]{64}$/.test(scope);
}

/**
 * Every construction and comparison of a ChatGPT scope goes through this module
 * and nowhere else. `chatGptWorkspaceScope` above is the one entry point for a
 * workspace value that may still be raw; the builders and predicates below are
 * its vocabulary for the marked (`fp1:`) and sentinel forms. A grep-based guard
 * (`tests/w301-one-scope-helper.test.ts`) fails if any other source file writes
 * the `chatgpt:` / `fp1:` prefix itself.
 */

/** The workspace-value form of an already-computed fingerprint digest: `fp1:<digest>`. */
export function fingerprintedChatGptWorkspace(fingerprint: string): FingerprintedChatGptWorkspace {
  return `fp1:${fingerprint}` as FingerprintedChatGptWorkspace;
}

/** The marked scope for an already-computed fingerprint digest. */
export function fingerprintedChatGptScope(fingerprint: string): string {
  return `chatgpt:${fingerprintedChatGptWorkspace(fingerprint)}`;
}

/** The digest shape inside a marked scope — the one place the 64-hex rule lives. */
export function isChatGptFingerprint(value: unknown): value is string {
  return typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
}

/** True when a stored value is a ChatGPT scope at all. */
export function isChatGptScope(scope: string): boolean {
  return scope.startsWith('chatgpt:');
}

/** The workspace part of a ChatGPT scope (raw id, `fp1:<digest>`, or a sentinel). */
export function chatGptScopeWorkspace(scope: string): string {
  return scope.slice('chatgpt:'.length);
}

/** True when a scope is one of the explicit unknown-workspace sentinels. */
export function isUnresolvedChatGptScope(scope: string): boolean {
  return scope.includes('!workspace-');
}

/** The storage scope for one explicit unknown-workspace reason. */
export function chatGptUnresolvedScope(reason: 'workspace-ambiguous' | 'workspace-unresolved'): string {
  return `chatgpt:!${reason}`;
}

/** True when a scope is the legacy `default` key, which names no workspace identity. */
export function isDefaultChatGptScope(scope: string): boolean {
  return scope === 'chatgpt:default';
}

/** True when a scope carries no workspace identity: the legacy default, a sentinel, or an empty workspace. */
export function isIdentitylessChatGptScope(scope: string): boolean {
  return isDefaultChatGptScope(scope)
    || scope === chatGptUnresolvedScope('workspace-unresolved')
    || scope === chatGptUnresolvedScope('workspace-ambiguous')
    || scope === 'chatgpt:';
}
