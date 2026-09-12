import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    // `#i18n` is the alias `@wxt-dev/i18n`'s WXT module registers
    // (node_modules/@wxt-dev/i18n/dist/module.mjs:80). WXT itself adds it to the
    // Vite build and to `.wxt/tsconfig.json`; vitest runs outside that build, so
    // it is repeated here. It points at the generated module, which is why tests
    // exercise the same `createI18n()` instance the extension ships.
    alias: {
      '#i18n': fileURLToPath(new URL('./.wxt/i18n/index.ts', import.meta.url)),
    },
  },
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
