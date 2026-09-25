#!/usr/bin/env node
// assemble.mjs — turn a release's assets into ready-to-publish npm package dirs.
//
// `npm/` in this repository is the source of two kinds of package:
//
//   chat-stasher                        the launcher (npm/package.json, npm/bin/)
//   @dimpurr/chat-stasher-<key>         one per platform, holding the binary
//                                       (npm/platforms/<key>/package.json)
//
// Neither is publishable as it stands: both are `private` and both carry the
// version placeholder 0.0.0, because the version a release publishes is the one
// in the git tag, not the one in the tree (crates/chat-stasher/Cargo.toml is
// 0.5.0-dev between releases, and that string must never reach a registry).
//
// So publishing goes through this script, which is given the version and the
// directory of release assets that GitHub Release already has, checks the
// binary's sha256 against that release's SHA256SUMS before copying it, and
// writes each package out with the version pinned in lockstep: main package,
// platform packages, and the optionalDependencies that tie them together all
// carry the same string, so npm can never resolve a launcher to a platform
// package from a different release.
//
// It does not publish. It writes directories; `npm publish` on them is a
// separate, deliberate step (RELEASING.md).
//
// Usage:
//   node scripts/npm/assemble.mjs --version 0.4.0 --assets dist --out dist/npm
//
//   --version V    required. The release's version, no leading `v`.
//   --assets DIR   required. The directory holding the release assets.
//   --out DIR      required. Where the package dirs are written.
//   --sha256sums P optional. Defaults to <assets>/SHA256SUMS.
//
// Exit codes: 0 = wrote every package · 1 = refused or failed · 2 = usage error.
//
// Re-running is safe and idempotent: the files it owns are overwritten with the
// same bytes. It never deletes anything — instead it refuses to leave a package
// dir holding a file it did not write, because a stale file in a publishable
// directory is indistinguishable from a built one at publish time.

import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const NPM_DIR = path.join(REPO, 'npm');

// The release asset holding the binary for a platform key. These are not derived
// from the key: npm spells the Intel architecture `x64` while the release
// artifacts spell it `x86_64` (scripts/release-artifacts.sh), so a template
// string would produce a 404 on Intel and nothing else would notice.
//
// The set of keys here and the set of directories under npm/platforms/ are
// asserted equal below, and npm/test/platform-packages.test.mjs asserts the same
// set against the launcher's own SUPPORTED list.
const RELEASE_ASSET = {
  'darwin-arm64': 'chat-stasher-darwin-arm64',
  'darwin-x64': 'chat-stasher-darwin-x86_64',
};

const MAIN_PACKAGE = 'chat-stasher';

const USAGE = [
  'usage: node scripts/npm/assemble.mjs --version V --assets DIR --out DIR [--sha256sums PATH]',
  '',
  '  --version V    required. The release version, without a leading "v".',
  '  --assets DIR   required. The directory holding the release assets.',
  '  --out DIR      required. Where the ready-to-publish package dirs are written.',
  '  --sha256sums P optional. Defaults to <assets>/SHA256SUMS.',
].join('\n');

function usage(message) {
  process.stderr.write(`assemble: ${message}\n`);
  process.stderr.write(`${USAGE}\n`);
  return 2;
}

function fail(message) {
  process.stderr.write(`assemble: ${message}\n`);
  return 1;
}

function parseArgs(argv) {
  const opts = { version: null, assets: null, out: null, sha256sums: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') return { help: true };
    const value = argv[i + 1];
    if (arg === '--version' || arg === '--assets' || arg === '--out' || arg === '--sha256sums') {
      if (value === undefined || value.startsWith('--')) {
        return { error: `${arg} needs a value` };
      }
      opts[arg.slice(2)] = value;
      i += 1;
    } else {
      return { error: `unknown argument: ${arg}` };
    }
  }
  for (const name of ['version', 'assets', 'out']) {
    if (opts[name] === null) return { error: `--${name} is required` };
  }
  if (opts.sha256sums === null) opts.sha256sums = path.join(opts.assets, 'SHA256SUMS');
  return { opts };
}

// The two ways a version may not be published, both of which have happened to
// someone: a version carrying the development suffix, and a version pasted
// straight off the git tag with its `v`.
function checkVersion(version) {
  if (version.includes('-dev')) {
    return `refusing to assemble a development version: ${version}`;
  }
  if (version.startsWith('v')) {
    return `version must not carry the tag's leading 'v': got ${version}, pass ${version.slice(1)}`;
  }
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    return `not a version this can pin: ${version} (want MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-pre)`;
  }
  return null;
}

function sha256(file) {
  return new Promise((resolve, reject) => {
    const hash = createHash('sha256');
    createReadStream(file)
      .on('error', reject)
      .on('data', (chunk) => hash.update(chunk))
      .on('end', () => resolve(hash.digest('hex')));
  });
}

// SHA256SUMS as `shasum -a 256` writes it: "<hex>  <name>" per line. A line
// this cannot read is an error, not a line to skip — skipping is how an asset
// ends up published with no checksum behind it.
function parseSums(text) {
  const sums = new Map();
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (line.trim() === '') continue;
    const match = /^([0-9a-fA-F]{64})[ \t]+\*?(.+?)[ \t]*$/.exec(line);
    if (match === null) {
      return { error: `SHA256SUMS line ${i + 1} is not a checksum line: ${JSON.stringify(line)}` };
    }
    const [, digest, name] = match;
    const previous = sums.get(name);
    if (previous !== undefined && previous !== digest.toLowerCase()) {
      return { error: `SHA256SUMS names ${name} twice with different digests` };
    }
    sums.set(name, digest.toLowerCase());
  }
  return { sums };
}

async function readJson(file) {
  return JSON.parse(await fs.readFile(file, 'utf8'));
}

async function writeJson(file, value) {
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, `${JSON.stringify(value, null, 2)}\n`);
}

// What a package dir is allowed to contain once this script has run. Anything
// else in there came from somewhere other than this run, and a publishable
// directory holding an unexplained file is worse than a failed build.
async function assertExactTree(dir, expected) {
  const found = [];
  async function walk(current) {
    for (const entry of await fs.readdir(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) await walk(full);
      else found.push(path.relative(dir, full));
    }
  }
  await walk(dir);
  const want = [...expected].sort();
  const have = found.sort();
  if (want.join('\n') !== have.join('\n')) {
    return `unexpected files in ${dir}:\n  want: ${want.join(', ')}\n  have: ${have.join(', ')}`;
  }
  return null;
}

async function copyShared(packageDir) {
  // Apache-2.0 section 4 requires the licence and NOTICE to travel with the
  // binary; npm includes LICENSE on its own but not NOTICE, which is why NOTICE
  // is also named in each template's `files`.
  await fs.copyFile(path.join(REPO, 'LICENSE'), path.join(packageDir, 'LICENSE'));
  await fs.copyFile(path.join(REPO, 'NOTICE'), path.join(packageDir, 'NOTICE'));
}

async function main(argv) {
  const parsed = parseArgs(argv);
  if (parsed.help) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  if (parsed.error) return usage(parsed.error);
  const { version, assets, out, sha256sums } = parsed.opts;

  const versionProblem = checkVersion(version);
  if (versionProblem !== null) return fail(versionProblem);

  // Writing the package dirs into the release-asset directory would put
  // directories among the assets, which is the one input a release must keep
  // exact (release.yml checks the asset set for equality).
  const relativeOut = path.relative(path.resolve(assets), path.resolve(out));
  if (relativeOut === '' || (!relativeOut.startsWith('..') && !path.isAbsolute(relativeOut))) {
    return fail(`--out is inside --assets: ${out} is under ${assets}`);
  }

  const assetDirs = new Set(Object.keys(RELEASE_ASSET));
  const templateDirs = new Set(await fs.readdir(path.join(NPM_DIR, 'platforms')));
  if (assetDirs.size !== templateDirs.size || [...assetDirs].some((key) => !templateDirs.has(key))) {
    return fail(
      `RELEASE_ASSET and npm/platforms/ list different platforms: `
      + `table [${[...assetDirs].sort().join(', ')}] vs templates [${[...templateDirs].sort().join(', ')}]`,
    );
  }

  const allAssets = await fs.readdir(assets).catch((err) => {
    if (err.code === 'ENOENT') return null;
    throw err;
  });
  if (allAssets === null) return fail(`no such asset directory: ${assets}`);
  if (allAssets.length === 0) return fail(`asset directory is empty: ${assets}`);

  let sums;
  try {
    sums = parseSums(await fs.readFile(sha256sums, 'utf8'));
  } catch (err) {
    if (err.code === 'ENOENT') return fail(`no such checksum file: ${sha256sums}`);
    throw err;
  }
  if (sums.error) return fail(sums.error);
  sums = sums.sums;

  // ---- the platform packages -------------------------------------------------
  const written = [];
  for (const key of Object.keys(RELEASE_ASSET).sort()) {
    const assetName = RELEASE_ASSET[key];
    const templateDir = path.join(NPM_DIR, 'platforms', key);
    const template = await readJson(path.join(templateDir, 'package.json'));
    const packageName = `@dimpurr/chat-stasher-${key}`;
    if (template.name !== packageName) {
      return fail(`npm/platforms/${key}/package.json is named ${template.name}, expected ${packageName}`);
    }

    const expected = sums.get(assetName);
    if (expected === undefined) {
      return fail(`${sha256sums} has no line for ${assetName} — refusing to ship a binary with no checksum behind it`);
    }
    const assetPath = path.join(assets, assetName);
    let actual;
    try {
      actual = await sha256(assetPath);
    } catch (err) {
      if (err.code === 'ENOENT') return fail(`release asset is not in ${assets}: ${assetName}`);
      throw err;
    }
    if (actual !== expected) {
      return fail(`${assetName} does not match its checksum\n  SHA256SUMS: ${expected}\n  on disk:    ${actual}`);
    }

    const packageDir = path.join(out, packageName);
    const binDir = path.join(packageDir, 'bin');
    await fs.mkdir(binDir, { recursive: true });
    await fs.copyFile(assetPath, path.join(binDir, MAIN_PACKAGE));
    await fs.chmod(path.join(binDir, MAIN_PACKAGE), 0o755);
    await copyShared(packageDir);

    const manifest = { ...template, version };
    delete manifest.private;
    await writeJson(path.join(packageDir, 'package.json'), manifest);

    const problem = await assertExactTree(packageDir, ['package.json', 'LICENSE', 'NOTICE', `bin/${MAIN_PACKAGE}`]);
    if (problem !== null) return fail(problem);

    written.push({ packageName, version, binPath: `bin/${MAIN_PACKAGE}`, bytes: (await fs.stat(assetPath)).size, sha256: actual });
  }

  // ---- the launcher package --------------------------------------------------
  const mainDir = path.join(out, MAIN_PACKAGE);
  const mainTemplate = await readJson(path.join(NPM_DIR, 'package.json'));
  if (mainTemplate.name !== MAIN_PACKAGE) {
    return fail(`npm/package.json is named ${mainTemplate.name}, expected ${MAIN_PACKAGE}`);
  }

  // Every platform package must be reachable from the launcher, and the launcher
  // must not name a package this script does not write: an optionalDependency
  // pointing at a package nobody publishes is a silent install-time hole, since
  // npm treats a failed optional dependency as a success.
  const declared = Object.keys(mainTemplate.optionalDependencies ?? {}).sort();
  const publishable = written.map((entry) => entry.packageName).sort();
  if (declared.join('\n') !== publishable.join('\n')) {
    return fail(
      `npm/package.json optionalDependencies and the platform packages disagree:\n`
      + `  optionalDependencies: ${declared.join(', ') || '(none)'}\n`
      + `  platform packages:    ${publishable.join(', ')}`,
    );
  }

  await fs.mkdir(path.join(mainDir, 'bin'), { recursive: true });
  await fs.copyFile(path.join(NPM_DIR, 'bin', `${MAIN_PACKAGE}.js`), path.join(mainDir, 'bin', `${MAIN_PACKAGE}.js`));
  await fs.chmod(path.join(mainDir, 'bin', `${MAIN_PACKAGE}.js`), 0o755);
  await fs.copyFile(path.join(NPM_DIR, 'README.md'), path.join(mainDir, 'README.md'));
  await copyShared(mainDir);

  // The lockstep: one version, pinned into the launcher's own version and into
  // every optionalDependency, from the single --version argument.
  const dependencies = {};
  for (const entry of written) dependencies[entry.packageName] = version;
  const mainManifest = { ...mainTemplate, version, optionalDependencies: dependencies };
  delete mainManifest.private;
  await writeJson(path.join(mainDir, 'package.json'), mainManifest);

  const problem = await assertExactTree(mainDir, ['package.json', 'README.md', 'LICENSE', 'NOTICE', `bin/${MAIN_PACKAGE}.js`]);
  if (problem !== null) return fail(problem);

  // ---- read back what was written -------------------------------------------
  // The tree is checked by reading it, not by trusting the code above: a mistake
  // in the lockstep is exactly the kind that produces a package that installs
  // fine and then cannot find its binary.
  const checked = [{ packageName: MAIN_PACKAGE, version, file: 'package.json' }];
  for (const entry of written) checked.push({ packageName: entry.packageName, version, file: 'package.json' });
  for (const entry of checked) {
    const manifestPath = path.join(out, entry.packageName, entry.file);
    const manifest = await readJson(manifestPath);
    if (manifest.version !== version) {
      return fail(`${manifestPath} reads version ${manifest.version}, expected ${version}`);
    }
    if ('private' in manifest) {
      return fail(`${manifestPath} still carries "private" — it could not be published`);
    }
    if (manifest.name !== entry.packageName) {
      return fail(`${manifestPath} is named ${manifest.name}, expected ${entry.packageName}`);
    }
    // npm treats a failed optionalDependency as a success, so this is the one
    // place the launcher-to-binary link can be checked from outside.
    if (entry.packageName === MAIN_PACKAGE) {
      for (const [name, pinned] of Object.entries(manifest.optionalDependencies ?? {})) {
        if (pinned !== version) {
          return fail(`${manifestPath} pins ${name} at ${pinned}, expected ${version}`);
        }
      }
    }
  }

  const lines = [`assemble: version ${version} pinned across ${checked.length} packages`];
  for (const entry of [...written].sort((a, b) => a.packageName.localeCompare(b.packageName))) {
    lines.push(`  ${entry.packageName} · ${entry.binPath} · ${entry.bytes} bytes · sha256 ${entry.sha256}`);
  }
  lines.push(`  ${MAIN_PACKAGE} · bin/${MAIN_PACKAGE}.js`);
  lines.push(`  out: ${path.resolve(out)}`);
  process.stdout.write(`${lines.join('\n')}\n`);
  return 0;
}

// A usage error exits 2, matching the rest of this repository's scripts, and a
// failure exits 1, so "you called it wrong" and "it did not work" stay apart.
//
// The entry runs only when this file is the script that was invoked.
// npm/test/platform-packages.test.mjs imports it to compare RELEASE_ASSET with
// the launcher's own platform list, and that import must not assemble anything.
const invokedAsScript = process.argv[1] !== undefined
  && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (invokedAsScript) {
  process.exitCode = await main(process.argv.slice(2));
}

export { RELEASE_ASSET, checkVersion, parseSums, main };
