'use strict';

// The npm side of pristine is four manifests that have to agree with each other and with the
// shim's target table. Nothing at runtime notices when they stop agreeing: npm installs a
// platform package from a different release, or none at all, and the wrapper execs whatever it
// finds. These assertions are the only thing standing between that and a release.

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');

const { TARGETS } = require('../lib/platform.cjs');

const WRAPPER_DIR = path.join(__dirname, '..');
const NPM_DIR = path.join(WRAPPER_DIR, '..');
const SCOPE = '@agentender/';

const wrapper = readManifest(path.join(WRAPPER_DIR, 'package.json'));

function readManifest(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

/** Where a platform package lives, derived from its published name. */
function directoryFor(packageName) {
  assert.ok(packageName.startsWith(SCOPE), `${packageName} is outside the published scope`);
  return path.join(NPM_DIR, packageName.slice(SCOPE.length));
}

test('the shim knows about exactly the packages the wrapper depends on', () => {
  assert.deepEqual(
    Object.keys(wrapper.optionalDependencies).sort(),
    TARGETS.map((target) => target.package).sort(),
  );
});

// `preserveMatchingDependencyRanges` defaults to true in Nx 22+, so a caret range that still
// matches the new version is left alone at release time. The wrapper would then ship pinned to
// a range npm can satisfy from an older release: a 0.2.0 wrapper with a 0.1.0 binary.
test('every platform pin is the wrapper version exactly, with no range', () => {
  for (const [name, pin] of Object.entries(wrapper.optionalDependencies)) {
    assert.equal(pin, wrapper.version, `${name} must be pinned to ${wrapper.version}`);
  }
});

test('every platform package exists on disk at the version the wrapper pins', () => {
  for (const target of TARGETS) {
    const manifest = readManifest(path.join(directoryFor(target.package), 'package.json'));
    assert.equal(manifest.name, target.package);
    assert.equal(manifest.version, wrapper.version);
  }
});

// `os` and `cpu` are what make npm install exactly one of these. Getting them wrong either
// installs none (the wrapper cannot find a binary) or several (wasted download, and the shim
// picks by its own table anyway).
test('every platform package declares the os and cpu its target names', () => {
  for (const target of TARGETS) {
    const manifest = readManifest(path.join(directoryFor(target.package), 'package.json'));
    assert.deepEqual(manifest.os, [target.platform]);
    assert.deepEqual(manifest.cpu, [target.arch]);
    if (target.libc) {
      assert.deepEqual(manifest.libc, [target.libc]);
    } else {
      assert.equal(manifest.libc, undefined, `${target.package} has no libc to declare`);
    }
  }
});

test('every platform package ships the binary the shim will look for', () => {
  for (const target of TARGETS) {
    const manifest = readManifest(path.join(directoryFor(target.package), 'package.json'));
    assert.ok(
      manifest.files.includes('pristine'),
      `${target.package} must pack the binary named "pristine"`,
    );
  }
});

// One shim, in the wrapper. A platform package that also declared `bin` would race the wrapper
// for the `pristine` name in `node_modules/.bin`, and which one won would depend on install order.
test('no platform package declares a bin of its own', () => {
  for (const target of TARGETS) {
    const manifest = readManifest(path.join(directoryFor(target.package), 'package.json'));
    assert.equal(manifest.bin, undefined, `${target.package} must not declare a bin`);
  }
});

// Scope item three of the design: the binaries are carried by the packages themselves, so
// installing works offline, behind a proxy, and with install scripts disabled. A lifecycle
// script anywhere here would quietly undo that.
test('nothing in the npm tree runs an install script', () => {
  const manifests = [wrapper, ...TARGETS.map((t) => readManifest(path.join(directoryFor(t.package), 'package.json')))];
  const lifecycle = ['preinstall', 'install', 'postinstall', 'prepare', 'prepack'];
  for (const manifest of manifests) {
    for (const script of lifecycle) {
      assert.equal(
        manifest.scripts?.[script],
        undefined,
        `${manifest.name} must not define a "${script}" script`,
      );
    }
  }
});

test('the wrapper points its bin at the shim it ships', () => {
  assert.deepEqual(wrapper.bin, { pristine: 'bin/pristine.cjs' });
  assert.ok(fs.existsSync(path.join(WRAPPER_DIR, wrapper.bin.pristine)));
  for (const entry of ['bin', 'lib']) {
    assert.ok(wrapper.files.includes(entry), `the wrapper must pack ${entry}/`);
  }
});
