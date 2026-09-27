/**
 * The multi-install suite's own configuration — EXT-9.
 *
 * 🔴 Why it is separate from `playwright.config.ts`, and not just a subset of it.
 *
 * `multi-install.spec.ts` is the only spec here that drives the **real
 * `chat-stasher` native host binary**: two persistent profiles, one host, and the
 * arbiter, the stage lock and the archive's provenance all exercised as shipped.
 * That makes a cargo build a prerequisite of the run, and `pnpm e2e` is the
 * command a contributor runs for an extension-only change — a compiler is a much
 * larger thing to require of it than the browser download that is already there.
 * So the suite is split rather than lengthened: `pnpm e2e` keeps its
 * prerequisites and its cost, and `pnpm e2e:multi` builds the host and runs what
 * needs it.
 *
 * Everything else matches the default config deliberately — `workers: 1`,
 * `retries: 0`, no trace, no video — because each of its reasons holds here too:
 * every case launches **two** browsers, and the assertion each one ends with is
 * that no request escaped the intercepted origins, which is only a statement
 * about this machine if this machine is the only one making them. A retry is how
 * a flaky capture path stops being information.
 *
 * `pnpm e2e:multi` from `apps/extension` builds the host binary and the extension
 * and then runs this config. The harness itself refuses to start a host it cannot
 * find, with the exact command that builds it, so a run without the binary fails
 * loudly rather than reporting an empty stage.
 */
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: /multi-install\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  // A `.only` left in a committed spec silently shrinks the suite to one case.
  forbidOnly: !!process.env.CI,
  // No retries, for the same reason as the default config: a flaky capture path
  // is information, and a retry is how that information gets discarded.
  retries: 0,
  reporter: [['list']],
  outputDir: './.results',
  // Two browser launches, two page loads and at least one real host round trip
  // per case; the cases that hold a request open while a second profile tries
  // also carry their own longer budget.
  timeout: 120_000,
  use: {
    trace: 'off',
    video: 'off',
  },
});
