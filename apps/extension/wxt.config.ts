import { defineConfig } from 'wxt';

export default defineConfig({
  manifest: {
    name: 'Chat Stasher',
    description: 'Capture selected web chat sessions to files on your own machine.',
    // Minimal permission set: only what the file-save channel needs.
    // No <all_urls>. Content-script matches stay on the explicit platform table.
    permissions: ['downloads'],
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
