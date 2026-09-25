// The lists of platforms have to agree, and nothing in the build makes them:
// the launcher decides at runtime from its own list, the templates under
// npm/platforms/ are what gets published, and RELEASE_ASSET in
// scripts/npm/assemble.mjs says which release asset each one is built from.
//
// A disagreement is not a loud failure. A key missing from the launcher's list
// makes a shipped platform report itself unsupported; an extra one there sends
// users to a package that was never published; a wrong asset name produces a
// package whose binary is the other architecture's. So the lists are compared
// here, where the mismatch is the test failure.
//
// Three more places name the same assets from outside this directory, and they
// are read from their own files rather than restated: the release workflow
// stages them, `cargo binstall` downloads them, and install.sh asks for the one
// that matches the running machine. None of those can be executed from a test,
// and all three fail the same silent way — a 404 at release time, or an
// installer that downloads nothing — so the names are compared as text.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

import launcher from '../bin/chat-stasher.js';
import { RELEASE_ASSET, binaryNameFor as assembledBinaryNameFor } from '../../scripts/npm/assemble.mjs';

const NPM_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const REPO = path.resolve(NPM_DIR, '..');
const read = (relative) => fs.readFileSync(path.join(REPO, relative), 'utf8');
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
    // The assets are named by the release workflow, which spells the Intel
    // architecture `x86_64` where npm spells it `x64` and the OS `windows`
    // where npm spells it `win32`. This asserts the table did not quietly copy
    // npm's spelling: that would 404 on exactly those two keys, and every test
    // that runs on a macOS host would still pass.
    assert.deepEqual(RELEASE_ASSET, {
      'darwin-arm64': 'chat-stasher-darwin-arm64',
      'darwin-x64': 'chat-stasher-darwin-x86_64',
      'linux-arm64': 'chat-stasher-linux-arm64',
      'linux-x64': 'chat-stasher-linux-x86_64',
      'win32-x64': 'chat-stasher-windows-x86_64.exe',
    });
  });

  // The names the assets have outside this directory, read from the files that
  // use them. Each extractor is written so that finding nothing is a failure:
  // a regex that stopped matching would otherwise make its comparison pass
  // against an empty set.
  const sortedSet = (iterable) => [...new Set(iterable)].sort();

  function stagedAssetNames() {
    // The workflow assigns each artifact name to a shell variable, in the build
    // job that produces it and again in the job that stages the whole set.
    const names = [...read('.github/workflows/release.yml')
      .matchAll(/^\s*ARTIFACT_[A-Z0-9_]+="(chat-stasher-[^"]+)"\s*$/gm)].map((match) => match[1]);
    assert.ok(names.length >= 5, `release.yml no longer assigns ARTIFACT_* names the way this expects (found ${names.length})`);
    return sortedSet(names);
  }

  function binstallAssetNames() {
    // One pkg-url per target, each ending in the asset it downloads.
    const names = [...read('crates/chat-stasher/Cargo.toml')
      .matchAll(/^pkg-url = "\{ repo \}\/releases\/download\/v\{ version \}\/(\S+)"\s*$/gm)].map((match) => match[1]);
    assert.ok(names.length >= 5, `Cargo.toml no longer declares binstall pkg-urls the way this expects (found ${names.length})`);
    return sortedSet(names);
  }

  function installerTargets() {
    // The one `case` arm that accepts a platform, e.g.
    // `darwin-arm64|darwin-x86_64|linux-x86_64|linux-arm64) : ;;`
    const arm = /^\s*([a-z0-9_]+-[a-z0-9_]+(?:\|[a-z0-9_]+-[a-z0-9_]+)+)\) : ;;/m.exec(read('scripts/install.sh'));
    assert.ok(arm !== null, 'install.sh no longer has a multi-platform accept arm this expects');
    return sortedSet(arm[1].split('|'));
  }

  it('stages exactly the assets this table names, with the same spelling', () => {
    assert.deepEqual(stagedAssetNames(), sortedSet(Object.values(RELEASE_ASSET)));
  });

  it('has a binstall pkg-url per asset, naming it exactly as it is published', () => {
    assert.deepEqual(binstallAssetNames(), sortedSet(Object.values(RELEASE_ASSET)));
  });

  it('has install.sh ask for an artifact that is really published', () => {
    // install.sh builds `chat-stasher-$(uname -s)-$(uname -m)` for the machine
    // it runs on, so every accepted target has to name a staged asset.
    const accepted = installerTargets();
    assert.deepEqual(accepted, ['darwin-arm64', 'darwin-x86_64', 'linux-arm64', 'linux-x86_64']);
    for (const target of accepted) {
      assert.ok(
        sortedSet(Object.values(RELEASE_ASSET)).includes(`chat-stasher-${target}`),
        `install.sh accepts ${target} but no such asset is released`,
      );
    }
    // Windows is not an accepted target — the installer refuses it — so the
    // one thing it must get right is the asset name it points at.
    const windowsAsset = Object.entries(RELEASE_ASSET)
      .filter(([key]) => key.startsWith('win32-'))
      .map(([, asset]) => asset);
    assert.equal(windowsAsset.length, 1);
    assert.ok(
      read('scripts/install.sh').includes(windowsAsset[0]),
      `install.sh must point a Windows user at ${windowsAsset[0]}`,
    );
  });

  it('agrees with the assembler about what the binary inside a package is called', () => {
    // Two implementations of one name: the launcher resolves it at run time,
    // the assembler writes it at publish time. A disagreement is a package
    // that installs cleanly and then reports its binary as unavailable, so the
    // two functions are compared here rather than trusted to stay in step.
    for (const key of TEMPLATE_KEYS) {
      assert.equal(launcher.binaryNameFor(key), assembledBinaryNameFor(key), key);
    }
    assert.equal(launcher.binaryNameFor('win32-x64'), 'chat-stasher.exe');
    assert.equal(launcher.binaryNameFor('linux-x64'), 'chat-stasher');
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
