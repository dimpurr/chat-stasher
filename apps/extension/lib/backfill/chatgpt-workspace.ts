/** Workspace identity observed from this page's own outgoing ChatGPT requests. */
export type ChatGptWorkspaceResolution =
  | { ok: true; workspace: string; observed: true }
  | { ok: false; reason: 'workspace-ambiguous' | 'workspace-unresolved'; observed: boolean };

export interface ChatGptWorkspaceObservation { accountIds: string[] }

export function observeChatGptAccountId(
  observation: ChatGptWorkspaceObservation,
  value: string | null | undefined,
): ChatGptWorkspaceObservation {
  const accountId = value?.trim();
  if (!accountId || accountId.length > 512 || observation.accountIds.includes(accountId) || observation.accountIds.length >= 2) return observation;
  return { accountIds: [...observation.accountIds, accountId] };
}

export function resolveChatGptWorkspace(observation: ChatGptWorkspaceObservation): ChatGptWorkspaceResolution {
  if (observation.accountIds.length > 1) return { ok: false, reason: 'workspace-ambiguous', observed: true };
  const workspace = observation.accountIds[0];
  return workspace
    ? { ok: true, workspace, observed: true }
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
  return fingerprint ? `chatgpt:${fingerprint}` : null;
}
