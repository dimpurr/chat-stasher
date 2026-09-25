// Shared scaffolding for the launcher tests.
//
// The launcher decides what to run from `process.platform` and `process.arch`,
// which a test cannot set for a process it spawns. So each test builds a
// throwaway directory holding
//
//   bin/chat-stasher.js            a copy of the launcher under test
//   bin/harness.js                 the same launcher with the platform forced
//   node_modules/@dimpurr/...      a fake platform package, when the test wants one
//
// and runs the harness. `require.resolve` walks up from bin/, so the fake package
// is found exactly the way a real install would be found, and the launcher's own
// resolution logic is what is being tested rather than a path a test handed it.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const LAUNCHER = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  'bin',
  'chat-stasher.js',
);

export const NPM_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// A platform package whose "binary" answers questions instead of doing work:
// argv as JSON on stdout, FAKE_STDERR on stderr, and a behaviour chosen by env
// so one file covers exit codes, signal deaths, and waiting to be signalled.
const FAKE_BINARY = `#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
process.stdout.write(JSON.stringify(process.argv.slice(2)) + '\\n');
if (process.env.FAKE_STDERR) process.stderr.write(process.env.FAKE_STDERR);
if (process.env.FAKE_SIGNAL) {
  process.kill(process.pid, process.env.FAKE_SIGNAL);
} else if (process.env.FAKE_WAIT_FOR_SIGNAL) {
  process.stdout.write('ready\\n');
  process.on('SIGTERM', () => {
    fs.writeFileSync(process.env.FAKE_MARKER, 'SIGTERM\\n');
    process.exit(0);
  });
  // The timer is both the keep-alive and the escape hatch. A signal listener
  // does not keep the event loop alive on its own, so without it the process
  // would exit 0 the moment it printed "ready" and the test would be measuring
  // an exit rather than a relay. With it, a run whose relay never arrives ends
  // in this exit code instead of a process that waits forever, which is the
  // difference between a test that fails and a test that hangs.
  setTimeout(() => process.exit(99), Number(process.env.FAKE_TIMEOUT_MS ?? '15000'));
} else {
  process.exit(Number(process.env.FAKE_EXIT ?? '0'));
}
`;

/**
 * Build a throwaway install layout.
 *
 * `platform` and `arch` are what the launcher will see. Pass `binary: false` to
 * leave the platform package present but its binary absent, or `package: false`
 * to leave the package out of node_modules entirely.
 */
export function makeFixture({ platform, arch, package: withPackage = true, binary: withBinary = true, executable = true } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'chat-stasher-test-'));
  const binDir = path.join(dir, 'bin');
  fs.mkdirSync(binDir, { recursive: true });
  fs.copyFileSync(LAUNCHER, path.join(binDir, 'chat-stasher.js'));
  fs.writeFileSync(
    path.join(binDir, 'harness.js'),
    `'use strict';\n`
    + `Object.defineProperty(process, 'platform', { value: ${JSON.stringify(platform)} });\n`
    + `Object.defineProperty(process, 'arch', { value: ${JSON.stringify(arch)} });\n`
    + `require('./chat-stasher.js').main();\n`,
  );

  if (withPackage) {
    const packageDir = path.join(dir, 'node_modules', '@dimpurr', `chat-stasher-${platform}-${arch}`);
    fs.mkdirSync(path.join(packageDir, 'bin'), { recursive: true });
    fs.writeFileSync(
      path.join(packageDir, 'package.json'),
      `${JSON.stringify({ name: `@dimpurr/chat-stasher-${platform}-${arch}`, version: '0.0.0' }, null, 2)}\n`,
    );
    if (withBinary) {
      // The name is written out here rather than taken from the launcher: the
      // fixture is the layout a test says exists, so a launcher that looked for
      // a different name would fail to find this file instead of agreeing with
      // itself. Windows runs a file only by its extension, so a win32 fixture
      // holds `chat-stasher.exe` — a shebang plus a .exe name is still a valid
      // executable on the hosts these tests run on, so the win32 layout is
      // covered without a Windows machine.
      const name = platform === 'win32' ? 'chat-stasher.exe' : 'chat-stasher';
      const binaryPath = path.join(packageDir, 'bin', name);
      fs.writeFileSync(binaryPath, FAKE_BINARY);
      fs.chmodSync(binaryPath, executable ? 0o755 : 0o644);
    }
  }

  return dir;
}

export function removeFixture(dir) {
  fs.rmSync(dir, { recursive: true, force: true });
}

// Run the harness and collect everything: the point of most of these tests is
// what the launcher did to a status, a stream, or a signal, seen from outside.
export function runLauncher(dir, argv, { env = {} } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [path.join(dir, 'bin', 'harness.js'), ...argv], {
      cwd: dir,
      env: { ...process.env, ...env },
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', (code, signal) => resolve({ code, signal, stdout, stderr }));
  });
}

// The spawn-and-collect shape above, for a caller that needs to signal the
// launcher mid-run. onSpawn gets the child so a test can wait for the fake
// binary's "ready" line before signalling.
export function runLauncherAndSignal(dir, argv, { signal, env = {}, readyOn = 'ready' } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [path.join(dir, 'bin', 'harness.js'), ...argv], {
      cwd: dir,
      env: { ...process.env, ...env },
    });
    let stdout = '';
    let stderr = '';
    let sent = false;
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      if (!sent && stdout.includes(readyOn)) {
        sent = true;
        child.kill(signal);
      }
    });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', (code, signalCode) => resolve({ code, signal: signalCode, stdout, stderr, sent }));
  });
}
