import { readFileSync } from 'node:fs';
import { defineConfig } from 'wxt';

import { buildStamp, buildVersion, parseBuildNumber, resolveReleaseChannel } from './lib/build-version';

export default defineConfig({
  // `@wxt-dev/i18n` (module source: node_modules/@wxt-dev/i18n/dist/module.mjs:10-93)
  // compiles `locales/<locale>.yml` into the `_locales/<locale>/messages.json`
  // catalog the browser ships with the extension. It also generates a typed
  // `#i18n` alias (module.mjs:80), which this extension deliberately does not
  // import — see the comment above `createI18n()` in lib/i18n.ts. It refuses to run at all
  // unless `manifest.default_locale` is set (module.mjs:18-21), which is why the
  // two lines below are a pair.
  modules: ['@wxt-dev/i18n/module'],
  /**
   * 🔴 W91 · **Where a build lands.**
   *
   * `.output` for every ordinary build and for releases. `CS_OUT_DIR` is a test
   * seam with exactly one caller — `tests/w91-build-channels.test.ts`, which
   * builds the stable and dev channels in one run and must not have the second
   * build overwrite the first's manifest. It changes no manifest byte.
   */
  outDir: process.env.CS_OUT_DIR ?? '.output',
  /**
   * 🔴 W59b · **The build stamp, folded into the bundle.**
   *
   * `runtime.getManifest().version` is not an identity on its own: it is `0.1.0` for
   * every build that does not go through `scripts/dev/reload-extension.sh`, so two
   * builds of different source share it and a halt record written by one is read by
   * the other as its own (lib/extension-build.ts has the full account). This define
   * is what makes the identity move with the build instead of with the version
   * number, and it is read in exactly one place — `bakedBuildStamp()`.
   *
   * 🔴 Computed inside the function, not at module scope, so a config re-evaluated by
   *    a fresh build produces a fresh stamp. `vitest.config.ts` defines the same key
   *    with a fixed test value, for the same reason `tests/i18n-harness.ts` fakes a
   *    manifest: a suite that ran without it would exercise the degraded
   *    configuration (a build that cannot tell itself from another) rather than the
   *    one a built extension runs with.
   *
   * 🔴 Nothing about the manifest changes here — `version` and `version_name` are
   *    still `buildVersion`'s, so an unversioned build's manifest stays byte-identical
   *    and `tests/build-version.test.ts` keeps meaning what it says.
   */
  vite: (env) => ({
    define: {
      __CS_BUILD_STAMP__: JSON.stringify(buildStamp(Date.now())),
      __CS_RELEASE_CHANNEL__: JSON.stringify(
        resolveReleaseChannel(process.env.CS_RELEASE_CHANNEL, env),
      ),
    },
  }),
  // `manifest` is a function so the version can reflect `CS_BUILD_NUMBER`
  // (see lib/build-version.ts). Without that variable the produced manifest is
  // byte-identical to one built with `version` left to WXT's package.json
  // default and no `version_name`. With it, the 4th version component is the
  // build number and `version_name` becomes `<semver>+build.<n>` — the signal
  // a dev reload cycle needs Chrome to notice a new version and re-inject
  // content scripts.
  manifest: () => {
    // The base version is the package.json `version` WXT would otherwise use.
    // Reading it here keeps the two sources from drifting.
    const baseVersion = JSON.parse(
      readFileSync(new URL('./package.json', import.meta.url), 'utf8'),
    ).version;
    const { version, versionName } = buildVersion(
      baseVersion,
      parseBuildNumber(process.env.CS_BUILD_NUMBER),
    );
    return {
      // `version_name` only appears on builds that carry a build number, so an
      // unversioned build ships an unadorned manifest.
      version,
      ...(versionName !== undefined ? { version_name: versionName } : {}),
      // The catalog is keyed by locale code, and `en` is both the file we generate
      // the compile-time types from (module.mjs:51-58) and the language we fall
      // back to when the browser asks for something we have not translated.
      default_locale: 'en',
    // 🔴 `__MSG_*__` is Chrome's manifest localization form — the browser
    // substitutes it from `_locales/<resolved locale>/messages.json` before the
    // extension ever sees the manifest. WXT ships the compiled catalog as a
    // public asset, so this needs no extra wiring here:
    // https://developer.chrome.com/docs/extensions/reference/manifest/name
    name: '__MSG_extName__',
    description: '__MSG_extDescription__',
    // Minimal permission set. No <all_urls>; content-script matches stay on the
    // explicit platform table in lib/contract.ts.
    //
    // 🔴 W2 removed 'downloads'. The extension no longer downloads anything:
    // captures go to the native host (or wait in the outbox), and the one-file
    // escape hatch is a Blob + <a download> click in the popup, which needs no
    // permission at all. 'downloads' was also the only permission in this list
    // that showed the user an install-time warning ("Manage your downloads.").
    //
    // Why 'storage': the debt set, the backfill switch, the backfill tick
    // record, the host status / pause records and the last-export stamp all
    // live in browser.storage.local, and Chrome requires this permission to
    // expose chrome.storage at all. The code fails closed without it
    // (tickBackfill returns 'no-store'; the engine halts with
    // 'storage-unavailable') rather than pretending to run. 'storage' shows no
    // install-time warning, so declaring it costs the user nothing.
    //
    // Why 'alarms' (C19): the backfill leg must have a heartbeat of its own,
    // otherwise a user who installs the extension and never opens the site
    // again would never finish backfilling. lib/backfill/alarm.ts documents the
    // 5-minute period and why. 'alarms' shows no install-time warning either.
    // Since W2 the alarm is also when the outbox is drained.
    //
    // Why 'nativeMessaging' (ADR-025): this is the delivery channel. The host
    // is the ordinary `chat-stasher` binary, registered by
    // `chat-stasher install-native-host --stage <path>`; the extension talks to
    // it with runtime.sendNativeMessage, one request per call. Without this
    // permission nothing can ever be archived, so it is not optional.
    //
    // Why 'unlimitedStorage' (W2): the outbox is an IndexedDB queue of
    // undelivered bundles (lib/outbox.ts, capped at 256 MiB by us). Chrome may
    // evict best-effort IndexedDB data under disk pressure -- which would mean
    // silently losing captures the user was told were queued. This permission
    // removes that eviction path. It is the honest counterpart of the §10
    // promise "the outbox never drops an item on its own".
    //
    // 🔴 Nothing else. No host permissions: the backfill leg fetches through
    // the content script that is ALREADY injected on these origins, as a
    // same-origin request in the user's own logged-in page context.
    permissions: ['unlimitedStorage', 'storage', 'alarms', 'nativeMessaging'],
    // 🔴 ADR-014: this pins the Chrome extension ID to
    // gihmdkkmmmkeiagjjiimacmgkdilofhi on every machine and every unpacked
    // install. Without it Chrome derives the id from the install *path*, so it
    // differs per machine — and a Native Messaging host manifest cannot list an
    // id it cannot predict. This is the PUBLIC half of the keypair; publishing
    // it is what pins the id and gives away nothing. The private half is not in
    // this repository and is not needed to build.
    key: 'MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAsBmAYgiVnBWzfija5tHUF69h8xsqiKpPe66WD8yGxRVyfJA27WZR37p/yTvqIJpWLf4HB68JuYhfvs97UY4umw1qX8/f/xsNkTjCkQAkIyrAKdvN7VoYtXtRYDIgtAgyCGjJL9/mzCYUPp2bAx/8om8M6FDN7KfUJ76fRADj6u3ajRdRLBmyZCeenA5/Eh9uqCWmNMnQt5EgIuHjaUaYGbpevjz53+9rZ3zitlUNbv6ZXA5A5QlUNv0ggA940UCdCa1C7ca3YPBqIOUSFIZZ1ezsI8klzi/A1zNpR4pSrSDYbVIfY2Qb1kaFVP/pMz4qhyMADwHrP/0pdLGFeMAdMwIDAQAB',
    browser_specific_settings: {
      gecko: {
        // Not a mailbox. AMO accepts either a GUID or "a string containing 80
        // characters or less formatted like an email address", and states that
        // "while you can use a real email address (remembering that this may
        // attract spam), any correctly formatted string can be used" —
        // https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/browser_specific_settings
        // (checked 2026-08-17). A non-mailbox string is the better choice here:
        // the id is published in every copy of the extension forever and cannot
        // be changed after release, so it should not be an address anyone has to
        // keep alive. Reporting contact lives in SECURITY.md instead.
        id: 'chat-stasher@team.iopho.com',
        // AMO gate since 2025-11-03 (new extensions): the extension collects
        // no user data, so the truthful declaration is "none".
        data_collection_permissions: { required: ['none'] },
      },
    },
    };
  },
});
