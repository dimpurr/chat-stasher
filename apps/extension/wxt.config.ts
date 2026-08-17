import { defineConfig } from 'wxt';

export default defineConfig({
  manifest: {
    name: 'Chat Stasher',
    description: 'Capture selected web chat sessions to files on your own machine.',
    // Minimal permission set: the file-save channel, plus the local storage the
    // debt set / backfill state / download guard are already built on.
    // No <all_urls>. Content-script matches stay on the explicit platform table.
    //
    // Why 'storage' is here: seven call sites under lib/ and entrypoints/ use
    // browser.storage.local (debt set, backfill enable flag, download-stall
    // guard, badge state). Chrome's extension docs require the "storage"
    // permission to expose chrome.storage at all. We have NOT empirically
    // verified what a real browser does when it is missing -- the code already
    // fails closed there (tickBackfill returns 'no-store', engine halts with
    // 'storage-unavailable') rather than pretending to run. Declaring it is
    // near-free: 'storage' shows no install-time warning to the user and needs
    // no separate justification beyond "the tool remembers what it still owes".
    // Omitting it risks every persistent thing silently not existing at runtime.
    // The cost is asymmetric, so we declare it.
    //
    // Why 'alarms' is here (C19): the backfill leg used to be kicked only when
    // the realtime leg captured something. That means a user who installs the
    // extension and never opens the site again would never finish backfilling
    // -- the product promise ("quietly finish over several days") cannot be
    // kept on that heartbeat alone. chrome.alarms gives it a heartbeat of its
    // own (lib/backfill/alarm.ts documents the 5-minute period and why).
    // Cost checked against Chrome's permission-warning list (2026-08-17):
    // 'alarms' shows NO install-time warning, same as 'storage'; the one that
    // does warn ("Manage your downloads.") is 'downloads', which we already
    // have. So the user-visible cost of this line is zero.
    // 🔴 Nothing else is added. No host permissions: the backfill leg fetches
    // through the content script that is ALREADY injected on these origins,
    // as a same-origin request in the user's own logged-in page context.
    permissions: ['downloads', 'storage', 'alarms'],
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
  },
});
