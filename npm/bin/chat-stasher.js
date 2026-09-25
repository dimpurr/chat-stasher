#!/usr/bin/env node
'use strict';

// The `chat-stasher` npm entry point.
//
// This package carries no binary of its own. The binary is in a per-platform
// package (`@dimpurr/chat-stasher-<platform>-<arch>`) named in
// `optionalDependencies`, and this file's whole job is to find the one for the
// running platform, run it, and relay what it does: argv, stdio, the exit
// status, and the signal it was killed by.
//
// The lookup is `require.resolve`, not a path assembled from a guess, because it
// is the only one that follows however the package manager actually laid
// node_modules out (npm, pnpm, a linked checkout, a hoisted tree).
//
// There are no dependencies, and nothing here runs at install time: this package
// has no postinstall script, so an install with `--ignore-scripts` still works.
//
// The three ways this can fail to find a binary are three different situations
// and are reported as three different things, because "your platform is not
// shipped" and "your platform is shipped but the package did not install" call
// for different next steps from the reader.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

// Every platform with a prebuilt binary, spelled the way `process.platform` and
// `process.arch` spell them. A key is also the package-name suffix, so
// `@dimpurr/chat-stasher-darwin-arm64` is the package for the key
// `darwin-arm64`; scripts/npm/assemble.mjs publishes one package per key and
// npm/test/platform-packages.test.mjs asserts the two lists agree.
const SUPPORTED = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64'];

// The command's own name, and the file the platform package holds it in.
const BINARY_NAME = 'chat-stasher';

// What the binary is called inside its platform package for a platform key.
//
// Windows needs the extension to run at all: `CreateProcess` appends `.exe`
// only when the name it was given has no extension, so a file actually named
// `chat-stasher` is not something it will start, and `fs.accessSync` would not
// see it either. Every other platform gets the bare name.
//
// scripts/npm/assemble.mjs writes the file this returns, and
// npm/test/platform-packages.test.mjs asserts the two agree — the failure this
// guards is a package that installs perfectly and then cannot be found.
function binaryNameFor(key) {
  return key.startsWith('win32-') ? `${BINARY_NAME}.exe` : BINARY_NAME;
}

// Where a reader can get a binary some other way. Both alternatives are in the
// same line on purpose: this is the one line an unsupported user sees.
const INSTEAD = 'Install with https://chatstasher.com/install.sh, or run: cargo install chat-stasher --locked';

// The name of the platform package that holds the binary for a platform key.
function packageNameFor(key) {
  return `@dimpurr/chat-stasher-${key}`;
}

/**
 * Decide what to run for a platform, without running anything.
 *
 * Returns `{ ok: true, key, packageName, binPath }`, or `{ ok: false, ... }`
 * with a `reason` of:
 *
 *   'unsupported-platform'   this key is not in SUPPORTED
 *   'package-not-installed'  the package exists but did not install here
 *   'binary-unavailable'     the package installed, the binary cannot be run
 *
 * `resolve` is `require.resolve` in production; tests pass their own to describe
 * an install layout without building one.
 */
function targetFor(platform, arch, resolve = require.resolve) {
  const key = `${platform}-${arch}`;
  const packageName = packageNameFor(key);

  if (!SUPPORTED.includes(key)) {
    return { ok: false, key, packageName, reason: 'unsupported-platform' };
  }

  let manifestPath;
  try {
    manifestPath = resolve(`${packageName}/package.json`);
  } catch (err) {
    // Most often MODULE_NOT_FOUND: npm treats a failed optionalDependency as a
    // success, so `--omit=optional`, an offline install, or a failed download
    // all land here with no warning shown to the user at install time.
    return { ok: false, key, packageName, reason: 'package-not-installed', errno: errnoOf(err) };
  }

  const binPath = path.join(path.dirname(manifestPath), 'bin', binaryNameFor(key));
  try {
    // X_OK, not exists: a binary that is present but not executable cannot be
    // run either, and saying "not installed" about it would be wrong.
    fs.accessSync(binPath, fs.constants.X_OK);
  } catch (err) {
    return { ok: false, key, packageName, reason: 'binary-unavailable', binPath, errno: errnoOf(err) };
  }

  return { ok: true, key, packageName, binPath };
}

// The errno name if the error carries one, and null if it does not. A caller
// that has no errno must say nothing rather than name a cause it did not see.
function errnoOf(err) {
  return err && typeof err.code === 'string' ? err.code : null;
}

// One line, naming which of the three situations this is. All three end the same
// way, with the two ways to get a binary that do not go through npm.
function messageFor(target) {
  const instead = INSTEAD;
  switch (target.reason) {
    case 'unsupported-platform':
      return `chat-stasher: no prebuilt binary for ${target.key} (prebuilt: ${SUPPORTED.join(', ')}). ${instead}`;
    case 'package-not-installed':
      return `chat-stasher: ${target.packageName} is not installed, so there is no binary for ${target.key}`
        + `${target.errno === null ? '' : ` (${target.errno})`}. ${instead}`;
    case 'binary-unavailable':
      return `chat-stasher: ${target.packageName} is installed but its binary cannot be run at ${target.binPath}`
        + `${target.errno === null ? '' : ` (${target.errno})`}. ${instead}`;
    default:
      // Unreachable while the three reasons above are the only ones targetFor
      // returns. It is phrased as a defect so that a fourth reason added without
      // a message here says so out loud instead of printing "undefined".
      return `chat-stasher: internal error, no message for reason ${String(target.reason)}. ${instead}`;
  }
}

// Signals this launcher relays to the child. A terminal sends SIGINT to the
// whole foreground process group, so the child usually sees it already; the
// relay is what covers `kill <launcher pid>`, which reaches only this process.
const FORWARDED_SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT'];

function runChild(binPath, argv) {
  const child = spawn(binPath, argv, { stdio: 'inherit' });

  const forwarders = new Map();
  for (const signal of FORWARDED_SIGNALS) {
    const forward = () => {
      try {
        child.kill(signal);
      } catch {
        // The child is already gone; its exit event is what this process waits on.
      }
    };
    forwarders.set(signal, forward);
    process.on(signal, forward);
  }

  child.on('error', (err) => {
    // The exec itself failed: a file that passed the access check a moment ago
    // can still be gone, on an unmounted volume, or past a length limit.
    process.stderr.write(`chat-stasher: cannot run ${binPath}: ${err.message}\n`);
    process.exit(1);
  });

  child.on('exit', (code, signal) => {
    // Dropped before a death signal is re-raised below: a listener installed for
    // a signal replaces the default disposition, so re-raising it while these
    // are still attached would invoke a forwarder and let this process exit 0.
    for (const [name, forward] of forwarders) process.removeListener(name, forward);

    if (signal !== null) {
      // Die of the same signal, so a caller that waits on the child sees a
      // signal death (and `$?` sees 128+n) rather than an exit code this
      // launcher made up.
      process.kill(process.pid, signal);
      // Reached only if that signal did not terminate this process, which is
      // possible when it is blocked or ignored. 128+n is the shell's own
      // spelling for a signal death, so the fallback stays in the same unit.
      const number = os.constants.signals[signal];
      process.exitCode = number === undefined ? 1 : 128 + number;
      return;
    }

    // `code` is null only for a signal death, handled above. Anything else that
    // is not a number is not a status to invent, so it becomes a failure.
    process.exitCode = typeof code === 'number' ? code : 1;
  });
}

function main() {
  const target = targetFor(process.platform, process.arch);
  if (!target.ok) {
    process.stderr.write(`${messageFor(target)}\n`);
    process.exitCode = 1;
    return;
  }
  runChild(target.binPath, process.argv.slice(2));
}

// Exported so the tests can drive the decision table and the spawn behaviour
// directly; the bin form is the branch below, not a second entry point.
module.exports = { SUPPORTED, packageNameFor, binaryNameFor, targetFor, messageFor, main };

if (require.main === module) main();
