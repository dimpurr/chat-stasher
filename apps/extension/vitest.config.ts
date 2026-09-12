import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'node',
    include: ['tests/**/*.test.ts'],
    server: {
      deps: {
        // 🔴 `@wxt-dev/browser` picks `globalThis.browser` or `globalThis.chrome`
        // **once, when it is first imported**, and `@wxt-dev/i18n` reads through
        // that captured object. Vitest externalises node_modules by default,
        // which caches those modules in Node's own loader where
        // `vi.resetModules()` cannot reach them — so a suite that swaps the
        // browser between cases would keep talking to the first one. Inlining
        // them puts them under vite's module runner, which `vi.resetModules()`
        // does reset, so "the browser this test stubbed" is the browser the
        // translation layer actually sees.
        inline: ['@wxt-dev/i18n', '@wxt-dev/browser'],
      },
    },
    // Installs the browser globals every suite needs (in particular the
    // `browser.i18n` that `@wxt-dev/browser` captures when it is first
    // imported), before any test module is loaded.
    setupFiles: ['tests/setup.ts'],
  },
});
