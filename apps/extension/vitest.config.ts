import { defineConfig } from 'vitest/config';

export default defineConfig({
  /**
   * 🔴 W59b · **The same build stamp `wxt.config.ts` folds in, pinned for the suite.**
   *
   * `__CS_BUILD_STAMP__` is read by `lib/extension-build.ts` and is what makes a
   * halt record's build identity distinguish two builds that report the same
   * manifest version. Without it here, every test would run the *degraded*
   * configuration — a build that cannot tell itself from another — and the rule
   * under test would be inert in the one place built to exercise it. That is the
   * same hole `tests/i18n-harness.ts` names when it explains why the fake manifest
   * exists at all.
   *
   * 🔴 The value is fixed rather than clock-derived so a test can name it, and
   *    `TEST_BUILD_STAMP` in tests/i18n-harness.ts is the other half of this one
   *    fact. If the two ever disagree the suite fails loudly on the halt stamps it
   *    compares — drift here is impossible to miss, not silent.
   */
  define: {
    __CS_BUILD_STAMP__: JSON.stringify('b-testsuite'),
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
