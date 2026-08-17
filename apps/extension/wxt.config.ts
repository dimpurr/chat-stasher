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
    permissions: ['downloads', 'storage'],
    browser_specific_settings: {
      gecko: {
        id: 'chat-stasher@local.spike',
        // AMO gate since 2025-11-03 (new extensions): the extension collects
        // no user data, so the truthful declaration is "none".
        data_collection_permissions: { required: ['none'] },
      },
    },
  },
});
