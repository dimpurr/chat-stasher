/**
 * The end-to-end suite's own configuration. It is deliberately separate from
 * `vitest.config.ts`: those tests run in Node with a stubbed `browser`, and
 * these run in a real Chromium with the built extension loaded. A failure in one
 * must never be reported as a failure in the other, so the two never share a
 * runner, a config, or a dependency graph.
 *
 * `pnpm e2e` from `apps/extension` builds first and then runs this config.
 * `workers: 1` is not a speed trade: every spec launches its own browser with
 * its own profile, and the assertion at the end of each one is that *no* request
 * escaped the two intercepted origins — which is only a statement about this
 * machine if this machine is the only one making them.
 */
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: /.*\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  // A `.only` left in a committed spec silently shrinks the suite to one case.
  forbidOnly: !!process.env.CI,
  // No retries. A flaky capture path is information, and a retry is how that
  // information gets discarded.
  retries: 0,
  reporter: [['list']],
  outputDir: './.results',
  // One browser launch plus one page load, per spec.
  timeout: 90_000,
  use: {
    trace: 'off',
    video: 'off',
  },
});
