#!/usr/bin/env node
/**
 * Every test file in the suite must run. One that does not is not a pass.
 *
 * ## Why this exists
 *
 * `tests/w32-dashboard-click.test.ts` declares `// @vitest-environment jsdom`.
 * Vitest sets a file's environment up *inside the worker*, by importing the
 * environment package from the vitest package's own location. When that import
 * fails, the file never becomes a test file at all: the pool reports
 * `Failed to start forks worker for test files .../w32-dashboard-click.test.ts`,
 * vitest drops it from the run, and every accounting surface that reads the
 * file list comes back clean —
 *
 *     Test Files  59 passed (59)
 *          Tests  677 passed (677)
 *        Errors  1 error
 *
 * with `numFailedTestSuites: 0` and **`success: true`** in the JSON report. The
 * file is not failed and not skipped; it is simply gone, and the summary counts
 * what ran rather than what exists. 677 is a measurement of the files that
 * chose to run, not of the suite.
 *
 * And that is the *loud* half. A test file the configuration stops selecting —
 * an `exclude` entry, a renamed file, a root that was dropped from `include` —
 * produces no error block at all: vitest exits 0, prints 59 files and no
 * complaint, and the suite is simply smaller than it looks.
 *
 * Both are the same failure this project refuses in the tool itself — an
 * unknown recorded as empty — and it is worth refusing here for the same
 * reason: a gate that can go green by running fewer tests is not a gate.
 *
 * ## What it compares, and why three sets
 *
 *   on disk   every file under `tests/` whose name ends `.test.ts`
 *   selected  `vitest list --filesOnly --json` — what the config's `include`
 *             matches, according to vitest itself
 *   executed  `vitest run --reporter=json` — what actually produced a result
 *
 * Any two of these would leave a hole. On-disk against executed alone cannot
 * tell "the config dropped this file" from "the worker dropped it", so the
 * failure would not name the right cause. Selected against executed alone is
 * blind to the case above, where the file never reaches either list — a
 * regression in this check's own first draft. All three together say which of
 * the two happened.
 *
 * The on-disk walk is the one place a glob is written down a second time, since
 * it is the only set vitest will not hand over. That transcription is kept
 * honest rather than hoped about: a file vitest selects from outside the walked
 * root fails this check by name, so the moment the two disagree the check says
 * so instead of quietly covering less. No count is hard-coded anywhere — adding
 * or removing a test file needs no edit here.
 *
 * ## Exit codes
 *
 * Following the project's rule that the code says *how* it ended:
 *
 *   0  compared, and every test file on disk ran
 *   1  compared, and it failed — files are missing, or vitest itself failed
 *   3  could not finish the comparison (no usable report or list), so any
 *      absence proves nothing. Never 0: not knowing is not passing.
 *
 * `pnpm test` runs this script, and this script runs vitest. Filters are
 * forwarded to both invocations, so `pnpm test -- tests/foo.test.ts` asks the
 * same question of both — and asks it of that one file only.
 */

import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const VITEST = join(ROOT, 'node_modules', 'vitest', 'vitest.mjs');
const FILTERS = process.argv.slice(2);

/**
 * The on-disk half of the comparison: the directory the suite lives in and the
 * suffix that marks a file as a test. Both mirror `include` in
 * `vitest.config.ts`; that file is the source of truth, and anything vitest
 * selects from outside this walk is reported below rather than ignored.
 */
const TESTS_DIR = join(ROOT, 'tests');
const TEST_SUFFIX = '.test.ts';

/** Exit codes, named so the call sites read as the rule they encode. */
const OK = 0;
const FAILED = 1;
const DID_NOT_FINISH = 3;

const scratch = mkdtempSync(join(tmpdir(), 'test-inventory-'));
const listFile = join(scratch, 'selected.json');
const reportFile = join(scratch, 'executed.json');

function runVitest(args) {
  return spawnSync(process.execPath, [VITEST, ...args], {
    cwd: ROOT,
    stdio: 'inherit',
  });
}

/** Parse a JSON file this script asked vitest to write, or `null` if it is not usable. */
function readJson(path) {
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

/** Every file under `dir` whose name ends with `TEST_SUFFIX`, as absolute paths. */
function testFilesOnDisk(dir) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch (error) {
    // An unreadable directory is "we could not look", not "there is nothing
    // there" — the two must not reach the same exit code.
    didNotFinish(`could not read ${dir} (${error.code ?? error.message})`);
  }
  const found = [];
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) found.push(...testFilesOnDisk(full));
    else if (entry.name.endsWith(TEST_SUFFIX)) found.push(full);
  }
  return found;
}

function finish(code, lines) {
  for (const line of lines) console.error(line);
  rmSync(scratch, { recursive: true, force: true });
  process.exit(code);
}

function didNotFinish(reason) {
  finish(DID_NOT_FINISH, [
    '',
    `check-test-inventory: could not carry out the check — ${reason}.`,
    'No conclusion is available from this run, so it does not pass: the absence',
    'of a result is not evidence that every test file ran.',
    '',
  ]);
}

/** Report a file the way the developer will look for it: relative to the extension. */
function show(file) {
  const path = relative(ROOT, file);
  return `  ${path.startsWith('..') ? file : path.split(sep).join('/')}`;
}

if (!existsSync(VITEST)) {
  didNotFinish(`the vitest entry point is missing at ${VITEST}`);
}

// 1. What does the configuration select? Asked of vitest, so `include` and
//    `exclude` are not transcribed here — a second copy of those rules would be
//    the first thing to rot, and it would rot silently.
//    `--filesOnly` stops at file names: collecting test names would import the
//    files, which is the very step this check exists to survive.
const list = runVitest(['list', '--filesOnly', '--json', listFile, ...FILTERS]);
if (list.status !== OK) {
  didNotFinish(`"vitest list" exited ${list.status ?? 'without a status'}`);
}

const selected = readJson(listFile);
if (!Array.isArray(selected)) {
  didNotFinish('"vitest list" wrote no usable file list');
}

// 2. What actually ran? The default reporter still prints, so the run reads as
//    it did before; the JSON copy is the machine-readable trace of it.
const run = runVitest([
  'run',
  '--reporter=default',
  '--reporter=json',
  `--outputFile.json=${reportFile}`,
  ...FILTERS,
]);

const report = readJson(reportFile);
if (report === null || !Array.isArray(report.testResults)) {
  // vitest exited non-zero for a reason it already printed (a config that will
  // not load, a filter matching nothing) and left no report to compare.
  // Propagate its code rather than inventing one — but never report success.
  if (run.status) finish(run.status, []);
  didNotFinish('"vitest run" wrote no usable test report');
}

// 3. Compare the three sets, by resolved path.
const asKey = (p) => resolve(p);
const executed = new Set(report.testResults.map((r) => asKey(r.name)));
const selectedSet = new Set(selected.map((entry) => asKey(entry.file)));

// The on-disk walk cannot see a filtered run: `pnpm test -- tests/foo.test.ts`
// is asking about one file, and the other sixty-nine are not missing from it.
// Absent a filter every file is in scope, so the walk is the whole suite.
const onDisk =
  FILTERS.length === 0
    ? testFilesOnDisk(TESTS_DIR).map(asKey)
    : selected.map((entry) => asKey(entry.file)).filter((file) => existsSync(file));

if (FILTERS.length === 0 && onDisk.length === 0) {
  // Nothing to compare against is a broken walk — a moved `tests/`, a suffix
  // that no longer matches — not an empty suite. Saying "all 0 files ran" here
  // would be the same substitution this whole check exists to refuse.
  didNotFinish(`no \`${TEST_SUFFIX}\` files were found under ${TESTS_DIR}`);
}

const unselected = onDisk.filter((file) => !selectedSet.has(file));
const unran = [...selectedSet].filter((file) => !executed.has(file));
const unasked = [...executed].filter((file) => !selectedSet.has(file));
const outsideWalk = [...selectedSet].filter(
  (file) => !onDisk.includes(file) && existsSync(file),
);

if (unselected.length > 0) {
  finish(FAILED, [
    '',
    `check-test-inventory: ${unselected.length} test ${
      unselected.length === 1 ? 'file is' : 'files are'
    } on disk but not selected by the configuration.`,
    '',
    ...unselected.map(show),
    '',
    'A file that is on disk and is not selected is not run, and vitest reports',
    'nothing: no failure, no skip, and a smaller suite that still exits 0. Check',
    '`exclude` and `include` in vitest.config.ts. If the file is meant to be',
    'outside the suite, its name is what should say so — a name that ends in',
    `\`${TEST_SUFFIX}\` claims to be a test.`,
    '',
  ]);
}

if (unran.length > 0) {
  finish(FAILED, [
    '',
    `check-test-inventory: ${unran.length} selected test ${
      unran.length === 1 ? 'file' : 'files'
    } never ran.`,
    '',
    ...unran.map(show),
    '',
    'A test file that does not run is not a pass, and vitest reports it as',
    'neither failed nor skipped: it leaves the run entirely, so the summary',
    'counts only the files that stayed. The usual cause is a file whose',
    '`@vitest-environment` package cannot be imported — for example a dev',
    'dependency that is declared but not installed — where the worker fails to',
    'start for that file and vitest drops it. Re-install dependencies and run',
    'again; if the file is still absent, vitest printed the reason above.',
    '',
  ]);
}

if (unasked.length > 0) {
  finish(FAILED, [
    '',
    `check-test-inventory: ${unasked.length} test ${
      unasked.length === 1 ? 'file' : 'files'
    } ran without being selected.`,
    '',
    ...unasked.map(show),
    '',
    'The file list and the run disagree about the same configuration, so the',
    'comparison this check is made of is not trustworthy until that is',
    'resolved.',
    '',
  ]);
}

if (outsideWalk.length > 0) {
  finish(FAILED, [
    '',
    `check-test-inventory: ${outsideWalk.length} selected test ${
      outsideWalk.length === 1 ? 'file is' : 'files are'
    } outside \`tests/\`.`,
    '',
    ...outsideWalk.map(show),
    '',
    'This check compares the run against the test files it can find on disk,',
    `and it only looks in \`tests/\` for names ending in \`${TEST_SUFFIX}\`. A file`,
    'outside that walk is covered by nothing, which is how this check would go',
    'quiet while covering less. Point the walk at wherever the suite now lives',
    '— or drop the new root from `include` — before trusting a green run.',
    '',
  ]);
}

// The inventory agrees: every file in scope was selected, and every selected
// file ran. The run's own exit code is still the verdict on the tests.
const scope = FILTERS.length === 0 ? 'test files on disk' : 'filtered test files';
if (run.status !== OK) {
  // Worded without "all N ran": with a filter that matched nothing, N is 0 and
  // that sentence would read as a pass. The inventory agreeing is not the same
  // claim as the tests passing, and this line must not blur the two.
  finish(run.status ?? DID_NOT_FINISH, [
    '',
    `check-test-inventory: the inventory agrees (${onDisk.length} ${scope}); ` +
      `vitest exited ${run.status}.`,
    '',
  ]);
}

console.log(`check-test-inventory: all ${onDisk.length} ${scope} ran.`);
finish(OK, []);
