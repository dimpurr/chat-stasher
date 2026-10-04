/** W337 · Resolve Claude's current user id at the page boundary. */

import { orgFromRequestUrl } from './backfill/claude-org';
import type { CapturedFetch } from './contract';

export const CLAUDE_WHOAMI_PATH = '/api/account';
export const CLAUDE_SETTINGS_PATH = '/api/claude_code/organizations';
export const CLAUDE_ACCOUNT_ID_MAX_CHARS = 512;

export type ClaudeAccountUnknownReason = 'no-account-id-in-capture' | 'account-id-unreadable';
export type ClaudeAccountReading =
  | { kind: 'id'; id: string; source: 'response-body-claude-whoami' | 'response-body-claude-user-settings' }
  | { kind: 'unknown'; reason: ClaudeAccountUnknownReason };

/** Replace any page-carried identity claim with a reading made by our own lookup. */
export function withClaudeAccountReading(
  captured: CapturedFetch,
  reading: ClaudeAccountReading,
): CapturedFetch {
  const untrustedFree = { ...captured };
  delete untrustedFree.claudeAccountId;
  delete untrustedFree.claudeAccountIdSource;
  delete untrustedFree.claudeAccountUnknownReason;
  return {
    ...untrustedFree,
    ...(reading.kind === 'id'
      ? { claudeAccountId: reading.id, claudeAccountIdSource: reading.source }
      : { claudeAccountUnknownReason: reading.reason }),
  };
}

export interface ClaudeIdentityResponse {
  status: number;
  text(): Promise<string>;
}

export type ClaudeIdentityFetch = (
  url: string,
  init: { method: 'GET'; credentials: 'same-origin'; cache: 'no-store'; headers: { accept: 'application/json' } },
) => Promise<ClaudeIdentityResponse>;

function validId(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const id = value.trim();
  return id.length > 0 && id.length <= CLAUDE_ACCOUNT_ID_MAX_CHARS ? id : null;
}

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

/** Read only current-user fields from a who-am-I response, never project/org ids. */
export function claudeWhoAmIId(value: unknown): string | null {
  const root = object(value);
  if (!root) return null;
  for (const key of ['userId', 'user_id']) {
    const id = validId(root[key]);
    if (id !== null) return id;
  }
  for (const owner of [object(root.user), object(root.currentUser), object(root.current_user)]) {
    if (owner) for (const key of ['userId', 'user_id', 'id', 'uuid']) {
      const id = validId(owner[key]);
      if (id !== null) return id;
    }
  }
  return null;
}

async function readJson(fetchImpl: ClaudeIdentityFetch, url: string): Promise<{ kind: 'json'; value: unknown } | { kind: 'missing' } | { kind: 'unreadable' }> {
  try {
    const response = await fetchImpl(url, {
      method: 'GET', credentials: 'same-origin', cache: 'no-store', headers: { accept: 'application/json' },
    });
    if (response.status === 404 || response.status === 405) return { kind: 'missing' };
    if (response.status < 200 || response.status > 299) return { kind: 'unreadable' };
    try { return { kind: 'json', value: JSON.parse(await response.text()) as unknown }; }
    catch { return { kind: 'unreadable' }; }
  } catch { return { kind: 'unreadable' }; }
}

/** Prefer a cache-disabled current-user endpoint, then Claude Code settings. */
export async function readClaudeAccountId(
  captureUrl: string,
  origin: string,
  fetchImpl: ClaudeIdentityFetch,
): Promise<ClaudeAccountReading> {
  let unreadable = false;
  const whoami = await readJson(fetchImpl, `${origin}${CLAUDE_WHOAMI_PATH}`);
  if (whoami.kind === 'json') {
    const id = claudeWhoAmIId(whoami.value);
    if (id !== null) return { kind: 'id', id, source: 'response-body-claude-whoami' };
    // A successful, readable response without a user id is a completed negative answer.
  } else if (whoami.kind === 'unreadable') unreadable = true;

  const org = orgFromRequestUrl(captureUrl);
  if (org === null) return { kind: 'unknown', reason: unreadable ? 'account-id-unreadable' : 'no-account-id-in-capture' };
  const settings = await readJson(fetchImpl, `${origin}${CLAUDE_SETTINGS_PATH}/${encodeURIComponent(org)}/user_settings`);
  if (settings.kind === 'json') {
    const root = object(settings.value);
    const id = validId(root?.userId);
    if (id !== null) return { kind: 'id', id, source: 'response-body-claude-user-settings' };
  } else if (settings.kind === 'unreadable') unreadable = true;
  return { kind: 'unknown', reason: unreadable ? 'account-id-unreadable' : 'no-account-id-in-capture' };
}
