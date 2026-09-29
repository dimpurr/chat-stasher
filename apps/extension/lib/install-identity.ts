/** Per-profile extension identity. Browser APIs deliberately expose no profile name. */
import { localStorageArea } from './local-storage';
import { resetReportSeq } from './report-seq';

export const INSTALL_IDENTITY_KEY = 'cs_install_identity_v1';

export interface InstallIdentity {
  install_id: string;
  browser: string;
  profile_label: string | null;
}

export function browserLabel(userAgent = globalThis.navigator?.userAgent ?? '', vendor = globalThis.navigator?.vendor ?? ''): string {
  if (/Edg\//.test(userAgent)) return 'Edge';
  if (/Brave\//.test(userAgent)) return 'Brave';
  if (/OPR\//.test(userAgent)) return 'Opera';
  if (/Firefox\//.test(userAgent)) return 'Firefox';
  if (/Vivaldi\//.test(userAgent)) return 'Vivaldi';
  if (/Arc\//.test(userAgent)) return 'Arc';
  if (vendor === 'Apple Computer, Inc.' && /Chrome\//.test(userAgent)) return 'Arc';
  if (/Chrome\//.test(userAgent)) return 'Chrome';
  return 'Chromium';
}

function newInstallId(): string | null {
  try {
    if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    bytes[6] = (bytes[6]! & 0x0f) | 0x40;
    bytes[8] = (bytes[8]! & 0x3f) | 0x80;
    const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  } catch {
    return null;
  }
}

let identityPromise: Promise<InstallIdentity> | null = null;

/** Missing or unreadable storage is an error; a capture must not invent an ephemeral identity. */
export function getInstallIdentity(): Promise<InstallIdentity> {
  if (identityPromise) return identityPromise;
  identityPromise = (async () => {
    const storage = localStorageArea();
    if (!storage) throw new Error('install identity storage unavailable');
    const query = { [INSTALL_IDENTITY_KEY]: null };
    const found = await storage.get(query);
    const saved = found[INSTALL_IDENTITY_KEY];
    if (saved && typeof saved === 'object') {
      const value = saved as Partial<InstallIdentity>;
      if (typeof value.install_id === 'string' && value.install_id.length > 0
        && typeof value.browser === 'string' && value.browser.length > 0
        && (value.profile_label === null || typeof value.profile_label === 'string')) {
        return { install_id: value.install_id, browser: value.browser, profile_label: value.profile_label };
      }
      throw new Error('install identity record unreadable');
    }
    const install_id = newInstallId();
    if (!install_id) throw new Error('install identity random source unavailable');
    const identity = { install_id, browser: browserLabel(), profile_label: null };
    await storage.set({ [INSTALL_IDENTITY_KEY]: identity });
    const confirmed = await storage.get(query);
    const persisted = confirmed[INSTALL_IDENTITY_KEY] as Partial<InstallIdentity> | undefined;
    if (persisted?.install_id !== install_id) throw new Error('install identity changed during initialization');
    return identity;
  })();
  return identityPromise;
}

/**
 * 🔴 ADR-045 §3 · **Give this profile its own identity** — the repair, and the
 * only way an `identity-conflict` is ever lifted.
 *
 * It is called from exactly one place: the popup button the user presses after
 * the host has reported that this install id is shared by more than one live
 * copy. Nothing on an automatic path calls it, and that is the decision, not an
 * oversight — the ADR rejects auto-rekey because a restored backup, a renamed
 * profile, or an ambiguous "later" copy would then have the tool silently sever
 * the wrong lineage, with the user reviewing nothing.
 *
 * What it does *not* do is as load-bearing as what it does: no already-sealed
 * record is touched, no status history is rewritten, and the old install id
 * keeps every capture and every report it was ever stamped on. The old id stays
 * conflicted in the host's own state — it is genuinely ambiguous — and this
 * profile's captures become attributable again under the new one.
 *
 * Order matters: the new id is written and confirmed **before** the sequence is
 * reset. Resetting first would leave a hole in the old id's sequence, and a
 * missing value reads to the host exactly like a regression.
 */
export async function rekeyInstallIdentity(): Promise<InstallIdentity> {
  const storage = localStorageArea();
  if (!storage) throw new Error('install identity storage unavailable');
  const current = await getInstallIdentity();
  const install_id = newInstallId();
  if (!install_id) throw new Error('install identity random source unavailable');
  const identity: InstallIdentity = {
    install_id,
    browser: current.browser,
    profile_label: current.profile_label,
  };
  await storage.set({ [INSTALL_IDENTITY_KEY]: identity });
  const confirmed = await storage.get({ [INSTALL_IDENTITY_KEY]: null });
  const persisted = confirmed[INSTALL_IDENTITY_KEY] as Partial<InstallIdentity> | undefined;
  if (persisted?.install_id !== install_id) {
    throw new Error('install identity changed during rekey');
  }
  identityPromise = Promise.resolve(identity);
  await resetReportSeq();
  return identity;
}

export async function setProfileLabel(label: string): Promise<InstallIdentity> {
  const cleaned = label.trim().slice(0, 80);
  if (!cleaned) throw new Error('profile label must not be empty');
  const identity = await getInstallIdentity();
  const updated = { ...identity, profile_label: cleaned };
  const storage = localStorageArea();
  if (!storage) throw new Error('install identity storage unavailable');
  await storage.set({ [INSTALL_IDENTITY_KEY]: updated });
  identityPromise = Promise.resolve(updated);
  return updated;
}

/** Add stable, content-free provenance to extension console lines after startup. */
export function attachInstallIdentityToConsole(identity: InstallIdentity): void {
  const g = globalThis as unknown as Record<string, unknown>;
  const marker = '__chatStasherInstallIdentityAttached';
  const contextKey = '__chatStasherInstallIdentityContext';
  const context = (g[contextKey] as Record<string, string> | undefined) ?? {
    install_id: identity.install_id,
    browser: identity.browser,
    profile_label: identity.profile_label ?? 'Name this profile',
  };
  Object.assign(context, {
    install_id: identity.install_id,
    browser: identity.browser,
    profile_label: identity.profile_label ?? 'Name this profile',
  });
  g[contextKey] = context;
  if (g[marker]) return;
  g[marker] = true;
  for (const method of ['log', 'info', 'warn', 'error', 'debug'] as const) {
    const original = console[method].bind(console);
    console[method] = (...args: unknown[]) => original(context, ...args);
  }
}
