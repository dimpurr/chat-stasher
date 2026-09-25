// The three lists of platforms have to agree, and nothing in the build makes
// them: the launcher decides at runtime from its own list, the templates under
// npm/platforms/ are what gets published, and RELEASE_ASSET in
// scripts/npm/assemble.mjs says which release asset each one is built from.
//
// A disagreement is not a loud failure. A key missing from the launcher's list
// makes a shipped platform report itself unsupported; an extra one there sends
// users to a package that was never published; a wrong asset name produces a
// package whose binary is the other architecture's. So the three are compared
// here, where the mismatch is the test failure.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

import launcher from '../bin/chat-stasher.js';
import { RELEASE_ASSET } from '../../scripts/npm/assemble.mjs';

const NPM_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const TEMPLATE_KEYS = fs.readdirSync(path.join(NPM_DIR, 'platforms'), { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();

const mainManifest = JSON.parse(fs.readFileSync(path.join(NPM_DIR, 'package.json'), 'utf8'));
const templateManifest = (key) => JSON.parse(fs.readFileSync(path.join(NPM_DIR, 'platforms', key, 'package.json'), 'utf8'));

describe('the platform lists', () => {
  it('names the same platforms in the launcher, the templates, and the release assets', () => {
    assert.deepEqual([...launcher.SUPPORTED].sort(), TEMPLATE_KEYS);
    assert.deepEqual(Object.keys(RELEASE_ASSET).sort(), TEMPLATE_KEYS);
  });

  it('has a release asset name per platform that says what the artifact says', () => {
    // The assets are named in scripts/release-artifacts.sh, which spells the
    // Intel architecture `x86_64` where npm spells it `x64`. This asserts the
    // table did not quietly copy npm's spelling, which would 404 on Intel only.
    assert.equal(RELEASE_ASSET['darwin-x64'], 'chat-stasher-darwin-x86_64');
    assert.equal(RELEASE_ASSET['darwin-arm64'], 'chat-stasher-darwin-arm64');
  });
});

describe('the platform package templates', () => {
  for (const key of TEMPLATE_KEYS) {
    describe(key, () => {
      // The key is `<os>-<cpu>` as npm spells them, split at the first hyphen.
      // A key with a second hyphen (a musl variant, say) fails here rather than
      // silently asserting the wrong pair.
      const separator = key.indexOf('-');
      const os = key.slice(0, separator);
      const cpu = key.slice(separator + 1);
      const manifest = templateManifest(key);

      it('is named after its platform key', () => {
        assert.equal(manifest.name, `@dimpurr/chat-stasher-${key}`);
      });

      it('restricts itself to the platform it holds a binary for', () => {
        assert.deepEqual(manifest.os, [os]);
        assert.deepEqual(manifest.cpu, [cpu]);
      });

      it('carries the licence and repository npm needs', () => {
        assert.equal(manifest.license, 'Apache-2.0');
        assert.equal(manifest.repository.url, 'git+https://github.com/dimpurr/chat-stasher.git');
      });

      it('has no install-time script at all', () => {
        // Not "no postinstall": no scripts whatever, so a future addition of one
        // is a decision this test makes visible.
        assert.equal(manifest.scripts, undefined);
      });

      it('exposes no bin, so the raw binary does not become a second command', () => {
        // The launcher owns the `chat-stasher` command. A bin field here would
        // link the same name to a binary that skips the launcher's checks.
        assert.equal(manifest.bin, undefined);
      });

      it('is marked private and unpluggable, and pins its version as a placeholder', () => {
        assert.equal(manifest.private, true, 'a publishable-in-place template can be published at 0.0.0');
        assert.equal(manifest.preferUnplugged, true, 'yarn PnP must extract a real binary');
        assert.equal(manifest.version, '0.0.0');
      });
    });
  }
});

describe('the launcher package', () => {
  it('is the unscoped package named chat-stasher', () => {
    assert.equal(mainManifest.name, 'chat-stasher');
    assert.equal(mainManifest.private, true);
    assert.equal(mainManifest.version, '0.0.0');
  });

  it('declares a bin that points at the launcher', () => {
    assert.deepEqual(mainManifest.bin, { 'chat-stasher': 'bin/chat-stasher.js' });
    assert.ok(fs.existsSync(path.join(NPM_DIR, mainManifest.bin['chat-stasher'])));
  });

  it('requires Node 18 or newer and has no dependencies', () => {
    assert.equal(mainManifest.engines.node, '>=18');
    assert.equal(mainManifest.dependencies, undefined);
  });

  it('has no install-time script', () => {
    assert.equal(mainManifest.scripts, undefined);
  });

  it('depends on exactly the platform packages, as optional dependencies', () => {
    assert.deepEqual(Object.keys(mainManifest.optionalDependencies).sort(), TEMPLATE_KEYS.map((key) => `@dimpurr/chat-stasher-${key}`).sort());
  });

  it('lets a platform with no binary install the launcher anyway', () => {
    // os/cpu on the launcher would make npm refuse the install on Linux, which
    // is the one case where the launcher has a message to print instead.
    assert.equal(mainManifest.os, undefined);
    assert.equal(mainManifest.cpu, undefined);
  });
});
