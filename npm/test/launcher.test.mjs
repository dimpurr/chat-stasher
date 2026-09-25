// Tests for npm/bin/chat-stasher.js, run with `node --test npm/test`.
//
// The unit tests drive the decision table directly. The process tests run the
// launcher for real against a fake platform package, because the three things
// this file exists to get right (an exit status, a signal, and a stream) are
// only observable from outside a process.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { after, describe, it } from 'node:test';

import { makeFixture, removeFixture, runLauncher, runLauncherAndSignal } from './helpers.mjs';

// A default import of a CommonJS module is its module.exports, whichever way the
// interop lexer reads the named ones.
import launcher from '../bin/chat-stasher.js';

const { SUPPORTED, messageFor, packageNameFor, binaryNameFor, targetFor } = launcher;

// A platform this package ships no binary for, used wherever a test needs one.
// It was `linux-x64` until Linux became a supported platform, at which point
// that case stopped testing "unshipped" and would have passed for the wrong
// reason. The property is what matters, so the example moved rather than the
// assertion.
const UNSHIPPED = { platform: 'freebsd', arch: 'x64', key: 'freebsd-x64' };

const fixtures = [];
function fixture(options) {
  const dir = makeFixture(options);
  fixtures.push(dir);
  return dir;
}
after(() => {
  for (const dir of fixtures) removeFixture(dir);
});

const notFound = () => {
  const err = new Error('Cannot find module');
  err.code = 'MODULE_NOT_FOUND';
  throw err;
};

describe('targetFor', () => {
  it('resolves the binary inside the platform package for the running platform', () => {
    // A real directory: the access check is part of the decision, so a path
    // invented by the test would only prove that a missing file is reported as
    // one. The layout is the one npm creates for an installed optional
    // dependency of the launcher package.
    const packageDir = path.join(fixture({ platform: 'darwin', arch: 'arm64' }), 'node_modules', '@dimpurr', 'chat-stasher-darwin-arm64');
    const target = targetFor('darwin', 'arm64', () => path.join(packageDir, 'package.json'));
    assert.deepEqual(target, {
      ok: true,
      key: 'darwin-arm64',
      packageName: '@dimpurr/chat-stasher-darwin-arm64',
      binPath: path.join(packageDir, 'bin', 'chat-stasher'),
    });
  });

  it('reports a platform with no package as unsupported, and does not look one up', () => {
    let looked = false;
    const target = targetFor(UNSHIPPED.platform, UNSHIPPED.arch, () => { looked = true; });
    assert.equal(target.ok, false);
    assert.equal(target.reason, 'unsupported-platform');
    assert.equal(looked, false);
  });

  it('tells an unsupported platform apart from a package that did not install', () => {
    assert.equal(targetFor(UNSHIPPED.platform, UNSHIPPED.arch, notFound).reason, 'unsupported-platform');
    assert.equal(targetFor('darwin', 'arm64', notFound).reason, 'package-not-installed');
  });

  it('looks for the name the binary actually has on each platform', () => {
    // Windows is the one platform where the file name is not the command name:
    // CreateProcess appends `.exe` only to a name that has no extension, so a
    // file literally called `chat-stasher` is not startable there.
    const dir = fixture({ platform: 'win32', arch: 'x64' });
    const manifestPath = path.join(dir, 'node_modules', '@dimpurr', 'chat-stasher-win32-x64', 'package.json');
    const target = targetFor('win32', 'x64', () => manifestPath);
    assert.equal(target.ok, true);
    assert.equal(target.binPath, path.join(path.dirname(manifestPath), 'bin', 'chat-stasher.exe'));

    for (const key of SUPPORTED) {
      const expected = key.startsWith('win32-') ? 'chat-stasher.exe' : 'chat-stasher';
      assert.equal(binaryNameFor(key), expected, `${key} resolves to ${expected}`);
    }
  });

  it('carries the errno when the package is missing, and null when there is none', () => {
    assert.equal(targetFor('darwin', 'arm64', notFound).errno, 'MODULE_NOT_FOUND');
    assert.equal(targetFor('darwin', 'arm64', () => { throw new Error('no code'); }).errno, null);
  });

  it('tells a package whose binary cannot be run apart from one that is absent', () => {
    // A real package directory, so the access check is what fails rather than the
    // resolution: a package that installed but whose binary is not there must not
    // be reported as one that never installed.
    const dir = fixture({ platform: 'darwin', arch: 'arm64', binary: false });
    const manifestPath = path.join(dir, 'node_modules', '@dimpurr', 'chat-stasher-darwin-arm64', 'package.json');
    const target = targetFor('darwin', 'arm64', () => manifestPath);
    assert.equal(target.ok, false);
    assert.equal(target.reason, 'binary-unavailable');
    assert.equal(target.binPath, path.join(path.dirname(manifestPath), 'bin', 'chat-stasher'));
    assert.equal(target.errno, 'ENOENT');
  });

  it('treats a binary without the execute bit as unavailable, not as installed and working', () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64', executable: false });
    const manifestPath = path.join(dir, 'node_modules', '@dimpurr', 'chat-stasher-darwin-arm64', 'package.json');
    const target = targetFor('darwin', 'arm64', () => manifestPath);
    assert.equal(target.ok, false);
    assert.equal(target.reason, 'binary-unavailable');
    assert.equal(target.errno, 'EACCES');
  });

  it('names the package after the platform key', () => {
    for (const key of SUPPORTED) {
      const [platform, arch] = key.split('-');
      assert.equal(packageNameFor(key), `@dimpurr/chat-stasher-${platform}-${arch}`);
    }
  });
});

describe('messageFor', () => {
  // Every message has to name the platform, and end with the two ways to install
  // that do not go through npm: this line is the whole user-visible surface of a
  // failed launch.
  const cases = [
    ['unsupported-platform', { key: UNSHIPPED.key }, UNSHIPPED.key],
    ['package-not-installed', { key: 'darwin-arm64', packageName: '@dimpurr/chat-stasher-darwin-arm64', errno: 'MODULE_NOT_FOUND' }, 'darwin-arm64'],
    ['binary-unavailable', { key: 'darwin-arm64', packageName: '@dimpurr/chat-stasher-darwin-arm64', binPath: '/x/bin/chat-stasher', errno: 'EACCES' }, 'darwin-arm64'],
  ];

  for (const [reason, fields, names] of cases) {
    it(`is one line for ${reason}, naming ${names}`, () => {
      const message = messageFor({ reason, ...fields });
      assert.equal(message.split('\n').length, 1);
      assert.match(message, new RegExp(names.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
      assert.ok(message.includes('https://chatstasher.com/install.sh'));
      assert.ok(message.includes('cargo install chat-stasher --locked'));
      assert.ok(message.startsWith('chat-stasher: '));
    });
  }

  it('still says something when a reason has no message', () => {
    const message = messageFor({ reason: 'a-reason-nobody-added-a-message-for' });
    assert.equal(message.split('\n').length, 1);
    assert.ok(message.includes('a-reason-nobody-added-a-message-for'));
  });
});

describe('running the binary', () => {
  it('passes the exit status through unchanged', async () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64' });
    for (const expected of [0, 1, 7, 42]) {
      const result = await runLauncher(dir, ['doctor'], { env: { FAKE_EXIT: String(expected) } });
      assert.equal(result.code, expected, `exit status ${expected} should arrive as ${expected}`);
      assert.equal(result.signal, null);
    }
  });

  it('passes argv through unchanged, including empty and dash-prefixed arguments', async () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64' });
    const argv = ['collect', '--json', '-v', '--since', '2026-01-01', '--', '--not-a-flag', ''];
    const result = await runLauncher(dir, argv);
    assert.equal(result.code, 0);
    assert.deepEqual(JSON.parse(result.stdout.trim()), argv);
  });

  it('runs the Linux binary and the Windows .exe of a fresh install', async () => {
    // Every supported platform key resolves through the same lookup, but these
    // two are the ones whose layout differs from the macOS case the rest of
    // this file uses (a different package name, and for win32 a different file
    // name inside it), so they are run rather than assumed.
    for (const [platform, arch] of [['linux', 'x64'], ['linux', 'arm64'], ['win32', 'x64']]) {
      const dir = fixture({ platform, arch });
      const result = await runLauncher(dir, ['doctor']);
      assert.equal(result.stderr, '', `${platform}-${arch} must not print a diagnostic`);
      assert.equal(result.code, 0, `${platform}-${arch} exited ${result.code}`);
      assert.deepEqual(JSON.parse(result.stdout.trim()), ['doctor']);
    }
  });

  it('passes both streams through', async () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64' });
    const result = await runLauncher(dir, ['status'], { env: { FAKE_STDERR: 'a warning on stderr\n' } });
    assert.equal(result.stderr, 'a warning on stderr\n');
    assert.deepEqual(JSON.parse(result.stdout.trim()), ['status']);
  });

  it('dies of the signal the binary died of, rather than reporting success', async () => {
    // The bug this guards: a launcher that assigns the child's status to its own
    // exit code propagates `null` for a signal death, which a shell reads as 0.
    const dir = fixture({ platform: 'darwin', arch: 'arm64' });
    const result = await runLauncher(dir, ['collect'], { env: { FAKE_SIGNAL: 'SIGTERM' } });
    assert.equal(result.signal, 'SIGTERM');
  });

  it('relays a signal it is given to the binary it started', async () => {
    // The fake binary writes the marker only if it is signalled, and otherwise
    // exits 99 after a wait; so this reports a launcher that did not relay as a
    // wrong exit code, not as a run that never ends.
    const dir = fixture({ platform: 'darwin', arch: 'arm64' });
    const marker = path.join(dir, 'signalled');
    const result = await runLauncherAndSignal(dir, ['collect'], {
      signal: 'SIGTERM',
      env: { FAKE_WAIT_FOR_SIGNAL: '1', FAKE_MARKER: marker, FAKE_TIMEOUT_MS: '10000' },
    });
    assert.equal(result.sent, true, 'the fake binary never announced it was ready');
    assert.equal(result.code, 0, 'the binary exited on the relayed signal, so the launcher should exit 0');
    assert.equal(fs.readFileSync(marker, 'utf8'), 'SIGTERM\n');
  });
});

describe('when there is no binary to run', () => {
  it('prints one line naming the unsupported platform and exits 1', async () => {
    const dir = fixture({ platform: UNSHIPPED.platform, arch: UNSHIPPED.arch });
    const result = await runLauncher(dir, ['doctor']);
    assert.equal(result.code, 1);
    assert.equal(result.stdout, '');
    assert.equal(result.stderr.trim().split('\n').length, 1);
    assert.match(result.stderr, new RegExp(`no prebuilt binary for ${UNSHIPPED.key}`));
    assert.match(result.stderr, /https:\/\/chatstasher\.com\/install\.sh/);
  });

  it('names the platform package that did not install, distinctly from an unsupported platform', async () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64', package: false });
    const result = await runLauncher(dir, ['doctor']);
    assert.equal(result.code, 1);
    assert.equal(result.stderr.trim().split('\n').length, 1);
    assert.match(result.stderr, /@dimpurr\/chat-stasher-darwin-arm64 is not installed/);
    assert.doesNotMatch(result.stderr, /no prebuilt binary/);
  });

  it('names the path when the package installed but its binary cannot be run', async () => {
    const dir = fixture({ platform: 'darwin', arch: 'arm64', binary: false });
    const result = await runLauncher(dir, ['doctor']);
    assert.equal(result.code, 1);
    assert.equal(result.stderr.trim().split('\n').length, 1);
    assert.match(result.stderr, /cannot be run at .*bin\/chat-stasher/);
    assert.doesNotMatch(result.stderr, /is not installed/);
  });
});
