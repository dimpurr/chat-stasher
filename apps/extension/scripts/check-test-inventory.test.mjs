import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, test } from 'node:test';
import { runCheck } from './check-test-inventory.mjs';

const fixtures = [];

afterEach(() => {
  for (const fixture of fixtures.splice(0)) {
    rmSync(fixture, { recursive: true, force: true });
  }
});

function makeFixture(t, { files = ['alpha.test.ts', 'beta.test.ts'], selected, executed } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'test-inventory-selftest-'));
  fixtures.push(root);
  const testsDir = join(root, 'tests');
  const scratchParent = join(root, 'scratch');
  mkdirSync(testsDir, { recursive: true });
  mkdirSync(scratchParent);
  for (const file of files) writeFileSync(join(testsDir, file), '// synthetic test');
  const statePath = join(root, 'fake-vitest-state.json');
  const selectedNames = selected ?? files;
  const executedNames = executed ?? selectedNames;
  writeFileSync(statePath, JSON.stringify({
    selected: selectedNames,
    executed: executedNames,
  }));
  const fakeVitest = join(root, 'fake-vitest.mjs');
  writeFileSync(fakeVitest, `
    import { readFileSync, writeFileSync } from 'node:fs';
    import { resolve } from 'node:path';
    const state = JSON.parse(readFileSync(${JSON.stringify(statePath)}, 'utf8'));
    const args = process.argv.slice(2);
    if (args[0] === 'list') {
      const output = args[args.indexOf('--json') + 1];
      writeFileSync(output, JSON.stringify(state.selected.map((file) => ({
        file: resolve(${JSON.stringify(root)}, 'tests', file),
      }))));
    } else {
      const outputArg = args.find((arg) => arg.startsWith('--outputFile.json='));
      const output = outputArg?.slice('--outputFile.json='.length);
      if (state.report === 'missing') process.exit(0);
      if (state.report === 'malformed') writeFileSync(output, '{');
      else writeFileSync(output, JSON.stringify({
        testResults: state.executed.map((file) => ({
          name: resolve(${JSON.stringify(root)}, 'tests', file),
        })),
      }));
    }
  `);
  return { root, testsDir, scratchParent, statePath, fakeVitest };
}

function run(fixture) {
  return runCheck({
    root: fixture.root,
    testsDir: fixture.testsDir,
    vitest: fixture.fakeVitest,
    scratchParent: fixture.scratchParent,
    filters: [],
  });
}

function assertScratchClean(fixture) {
  assert.deepEqual(readdirSync(fixture.scratchParent), []);
}

test('passes when every on-disk file is selected and executed, and removes scratch output', (t) => {
  const fixture = makeFixture(t);
  const result = run(fixture);
  assert.equal(result.code, 0);
  assert.match(result.stdout, /all 2 test files on disk ran/);
  assert.equal(result.stderr, '');
  assertScratchClean(fixture);
});

test('fails with exit 1 and names an on-disk test omitted by include', (t) => {
  const fixture = makeFixture(t, { selected: ['alpha.test.ts'], executed: ['alpha.test.ts'] });
  const result = run(fixture);
  assert.equal(result.code, 1);
  assert.deepEqual(
    result.stderr.match(/^  tests\/.*\.test\.ts$/gm),
    ['  tests/beta.test.ts'],
  );
  assertScratchClean(fixture);
});

test('fails with exit 1 and names a selected test dropped by the worker', (t) => {
  const fixture = makeFixture(t, { executed: ['alpha.test.ts'] });
  const result = run(fixture);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /never ran/);
  assert.deepEqual(
    result.stderr.match(/^  tests\/.*\.test\.ts$/gm),
    ['  tests/beta.test.ts'],
  );
  assertScratchClean(fixture);
});

test('returns exit 3 and cleans scratch output when the JSON report is malformed or missing', (t) => {
  for (const report of ['malformed', 'missing']) {
    const fixture = makeFixture(t);
    const state = JSON.parse(readFileSync(fixture.statePath, 'utf8'));
    state.report = report;
    writeFileSync(fixture.statePath, JSON.stringify(state));
    const result = run(fixture);
    assert.equal(result.code, 3, `${report} report must be an unknown result`);
    assert.match(result.stderr, /no usable test report/);
    assertScratchClean(fixture);
  }
});
