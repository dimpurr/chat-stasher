// Tests for scripts/npm/assemble.mjs, run with `node --test npm/test`.
//
// Every test here is about what the script refuses: the packages it writes are
// what a release publishes, and the failure that matters is not "it wrote the
// wrong bytes" (the copy is the checksum-checked one) but "it wrote them
// anyway" — from an asset that did not match, from a missing checksum, from a
// development version, or into a directory that already held something.
//
// The release assets are fixtures, not downloads: the script's only inputs are a
// file's bytes and its sha256, so a two-line fixture exercises the same code a
// 43 MB Mach-O does.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

import { checkVersion, parseSums } from '../../scripts/npm/assemble.mjs';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const ASSEMBLE = path.join(REPO, 'scripts', 'npm', 'assemble.mjs');

const ARM64 = 'chat-stasher-darwin-arm64';
const X64 = 'chat-stasher-darwin-x86_64';
const LINUX_ARM64 = 'chat-stasher-linux-arm64';
const LINUX_X64 = 'chat-stasher-linux-x86_64';
const WINDOWS_X64 = 'chat-stasher-windows-x86_64.exe';

// Every package a release publishes, in the order the assertions below list
// them: the launcher first, then one per platform key.
const PACKAGES = [
  'chat-stasher',
  '@dimpurr/chat-stasher-darwin-arm64',
  '@dimpurr/chat-stasher-darwin-x64',
  '@dimpurr/chat-stasher-linux-arm64',
  '@dimpurr/chat-stasher-linux-x64',
  '@dimpurr/chat-stasher-win32-x64',
];

const temporary = [];
function tmpdir(prefix) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  temporary.push(dir);
  return dir;
}
after(() => {
  for (const dir of temporary) fs.rmSync(dir, { recursive: true, force: true });
});

const sha256 = (buffer) => createHash('sha256').update(buffer).digest('hex');

// A release directory with one binary per platform and a SHA256SUMS over them.
// `missing` names assets that get a checksum line but no file, which is the
// "the checksum says it should be here" case; `sums` replaces the checksum file.
function release({ missing = [], sums } = {}) {
  const dir = tmpdir('chat-stasher-release-');
  const bytes = {
    [ARM64]: Buffer.from('fixture bytes for the arm64 binary\n'),
    [X64]: Buffer.from('fixture bytes for the x86_64 binary\n'),
    [LINUX_ARM64]: Buffer.from('fixture bytes for the linux arm64 binary\n'),
    [LINUX_X64]: Buffer.from('fixture bytes for the linux x86_64 binary\n'),
    [WINDOWS_X64]: Buffer.from('fixture bytes for the windows binary\n'),
  };
  for (const [name, buffer] of Object.entries(bytes)) {
    if (!missing.includes(name)) fs.writeFileSync(path.join(dir, name), buffer);
  }
  const lines = Object.entries(bytes).map(([name, buffer]) => `${sha256(buffer)}  ${name}`);
  fs.writeFileSync(path.join(dir, 'SHA256SUMS'), sums ?? `${lines.join('\n')}\n`);
  return dir;
}

function assemble(argv) {
  const result = spawnSync(process.execPath, [ASSEMBLE, ...argv], { cwd: REPO, encoding: 'utf8' });
  return { code: result.status, stdout: result.stdout, stderr: result.stderr };
}

const run = (assets, out, extra = []) => assemble(['--version', '0.4.0', '--assets', assets, '--out', out, ...extra]);

function readManifest(out, packageName) {
  return JSON.parse(fs.readFileSync(path.join(out, packageName, 'package.json'), 'utf8'));
}

describe('assembling a release', () => {
  it('writes one package per platform plus the launcher, all at the given version', () => {
    const assets = release();
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    const result = run(assets, out);
    assert.equal(result.code, 0, result.stderr);

    for (const name of PACKAGES) {
      const manifest = readManifest(out, name);
      assert.equal(manifest.version, '0.4.0', `${name} must carry the release version`);
      assert.equal(manifest.private, undefined, `${name} must be publishable, not private`);
    }

    // The lockstep that stops npm pairing a launcher with another release's
    // binary: every optionalDependency is pinned at the launcher's own version.
    assert.deepEqual(readManifest(out, 'chat-stasher').optionalDependencies, {
      '@dimpurr/chat-stasher-darwin-arm64': '0.4.0',
      '@dimpurr/chat-stasher-darwin-x64': '0.4.0',
      '@dimpurr/chat-stasher-linux-arm64': '0.4.0',
      '@dimpurr/chat-stasher-linux-x64': '0.4.0',
      '@dimpurr/chat-stasher-win32-x64': '0.4.0',
    });
  });

  it('copies the checked bytes, executable, with the licence alongside', () => {
    const assets = release();
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    assert.equal(run(assets, out).code, 0);

    const binary = path.join(out, '@dimpurr/chat-stasher-darwin-arm64', 'bin', 'chat-stasher');
    assert.equal(sha256(fs.readFileSync(binary)), sha256(fs.readFileSync(path.join(assets, ARM64))));
    assert.notEqual(fs.statSync(binary).mode & 0o111, 0, 'the binary must be executable');

    // Two of the asset names are spelled unlike their npm keys (`x86_64` where
    // npm says `x64`, `windows` where npm says `win32`), so each copy is
    // checked against the asset it must have come from rather than against the
    // one next to it.
    for (const [packageName, assetName] of [
      ['@dimpurr/chat-stasher-darwin-x64', X64],
      ['@dimpurr/chat-stasher-linux-x64', LINUX_X64],
      ['@dimpurr/chat-stasher-linux-arm64', LINUX_ARM64],
    ]) {
      const copied = path.join(out, packageName, 'bin', 'chat-stasher');
      assert.equal(
        sha256(fs.readFileSync(copied)),
        sha256(fs.readFileSync(path.join(assets, assetName))),
        `${packageName} must hold ${assetName}`,
      );
    }

    // Windows gets the extension: a file named `chat-stasher` is not something
    // Windows will start, so the package would install and then be unusable.
    const winBinary = path.join(out, '@dimpurr/chat-stasher-win32-x64', 'bin', 'chat-stasher.exe');
    assert.equal(sha256(fs.readFileSync(winBinary)), sha256(fs.readFileSync(path.join(assets, WINDOWS_X64))));
    assert.ok(!fs.existsSync(path.join(out, '@dimpurr/chat-stasher-win32-x64', 'bin', 'chat-stasher')));

    for (const name of PACKAGES) {
      for (const file of ['LICENSE', 'NOTICE']) {
        assert.ok(fs.existsSync(path.join(out, name, file)), `${name} is missing ${file}`);
      }
    }
    assert.ok(fs.existsSync(path.join(out, 'chat-stasher', 'bin', 'chat-stasher.js')));
    assert.ok(fs.existsSync(path.join(out, 'chat-stasher', 'README.md')));
  });

  it('can be run again over its own output', () => {
    const assets = release();
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    assert.equal(run(assets, out).code, 0);
    const second = run(assets, out);
    assert.equal(second.code, 0, second.stderr);
    assert.equal(readManifest(out, 'chat-stasher').version, '0.4.0');
  });

  it('writes the launcher the repository actually ships', () => {
    const assets = release();
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    assert.equal(run(assets, out).code, 0);
    assert.deepEqual(
      fs.readFileSync(path.join(out, 'chat-stasher', 'bin', 'chat-stasher.js')),
      fs.readFileSync(path.join(REPO, 'npm', 'bin', 'chat-stasher.js')),
    );
  });
});

describe('what assembling refuses', () => {
  it('refuses a development version', () => {
    const result = run(release(), path.join(tmpdir('chat-stasher-out-'), 'npm'), ['--version', '0.5.0-dev']);
    assert.equal(result.code, 1);
    assert.match(result.stderr, /refusing to assemble a development version: 0\.5\.0-dev/);
  });

  it('refuses a version pasted off the git tag with its v', () => {
    const result = run(release(), path.join(tmpdir('chat-stasher-out-'), 'npm'), ['--version', 'v0.4.0']);
    assert.equal(result.code, 1);
    assert.match(result.stderr, /leading 'v'/);
  });

  it('refuses an asset that does not match its checksum, and names both digests', () => {
    // A fresh release, whose checksum line was computed over good bytes, with
    // the file then replaced by different ones: a truncated or tampered
    // download, which is the case the check exists for.
    const assets = release();
    fs.writeFileSync(path.join(assets, ARM64), 'tampered bytes\n');
    const result = run(assets, path.join(tmpdir('chat-stasher-out-'), 'npm'));
    assert.equal(result.code, 1);
    assert.match(result.stderr, new RegExp(`${ARM64} does not match its checksum`));
    assert.match(result.stderr, /SHA256SUMS: [0-9a-f]{64}/);
    assert.match(result.stderr, /on disk: {4}[0-9a-f]{64}/);
  });

  it('refuses an asset the checksum file does not cover', () => {
    // An asset with no line in SHA256SUMS is not an asset with no checksum to
    // check; it is one nobody verified, so it is not shipped.
    const assets = release();
    const kept = fs.readFileSync(path.join(assets, 'SHA256SUMS'), 'utf8')
      .split('\n').filter((line) => !line.includes(X64)).join('\n');
    fs.writeFileSync(path.join(assets, 'SHA256SUMS'), kept);
    const result = run(assets, path.join(tmpdir('chat-stasher-out-'), 'npm'));
    assert.equal(result.code, 1);
    assert.match(result.stderr, new RegExp(`no line for ${X64}`));
  });

  it('refuses a checksum file it cannot read rather than skipping the line', () => {
    const assets = release({ sums: `${sha256(Buffer.from('x'))}  ${ARM64}\nnot a checksum line\n` });
    const result = run(assets, path.join(tmpdir('chat-stasher-out-'), 'npm'));
    assert.equal(result.code, 1);
    assert.match(result.stderr, /SHA256SUMS line 2 is not a checksum line/);
  });

  it('refuses a release asset that is not there', () => {
    const assets = release({ missing: [X64] });
    const result = run(assets, path.join(tmpdir('chat-stasher-out-'), 'npm'));
    assert.equal(result.code, 1);
    assert.match(result.stderr, new RegExp(`release asset is not in .*: ${X64}`));
  });

  it('refuses to write the packages into the release assets', () => {
    const assets = release();
    const result = run(assets, path.join(assets, 'npm'));
    assert.equal(result.code, 1);
    assert.match(result.stderr, /--out is inside --assets/);
  });

  it('refuses a package directory holding a file it did not write', () => {
    // A stale file in a publishable directory is indistinguishable from a built
    // one at publish time, so it stops the run instead of being ignored.
    const assets = release();
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    fs.mkdirSync(path.join(out, 'chat-stasher'), { recursive: true });
    fs.writeFileSync(path.join(out, 'chat-stasher', 'left-over.txt'), 'from an earlier layout\n');
    const result = run(assets, out);
    assert.equal(result.code, 1);
    assert.match(result.stderr, /unexpected files in .*chat-stasher/);
    assert.match(result.stderr, /left-over\.txt/);
  });

  it('exits 2 for a usage error, separately from a refusal', () => {
    const out = path.join(tmpdir('chat-stasher-out-'), 'npm');
    const missing = assemble(['--version', '0.4.0', '--assets', release()]);
    assert.equal(missing.code, 2);
    assert.match(missing.stderr, /--out is required/);

    const unknown = assemble(['--version', '0.4.0', '--assets', release(), '--out', out, '--publish']);
    assert.equal(unknown.code, 2);
    assert.match(unknown.stderr, /unknown argument: --publish/);
  });
});

describe('checkVersion', () => {
  it('accepts a release and a release candidate', () => {
    for (const version of ['0.4.0', '1.0.0', '0.5.0-rc.1']) {
      assert.equal(checkVersion(version), null, `${version} should be assemblable`);
    }
  });

  it('rejects the development suffix, the tag prefix, and anything else', () => {
    assert.match(checkVersion('0.5.0-dev'), /development version/);
    assert.match(checkVersion('v0.4.0'), /leading 'v'/);
    assert.match(checkVersion('0.4'), /not a version/);
    assert.match(checkVersion(''), /not a version/);
  });
});

describe('parseSums', () => {
  it('reads the shasum format, including a binary-mode asterisk', () => {
    const digest = sha256(Buffer.from('x'));
    const { sums } = parseSums(`${digest}  one\n${digest} *two\n\n`);
    assert.deepEqual([...sums.entries()], [['one', digest], ['two', digest]]);
  });

  it('refuses one name with two digests', () => {
    const { error } = parseSums(`${sha256(Buffer.from('x'))}  same\n${sha256(Buffer.from('y'))}  same\n`);
    assert.match(error, /names same twice with different digests/);
  });
});
