#!/usr/bin/env node
// Proves the npm wrapper end to end on the host that runs it: build the binary, stage it into
// this platform's package, pack both, install the tarballs into a scratch project, and run the
// `pristine` that npm put on the PATH.
//
// Reading the manifests cannot prove any of this. The executable bit surviving `npm pack`, npm
// honouring `os`/`cpu`, the shim resolving a package it does not depend on directly, and the
// binary's own version agreeing with the wrapper's are all properties of the installed tree.
//
// Run it with `nx run npm-pristine:verify-pack`. It touches nothing outside `npm/pristine-*/`
// (one ignored binary) and a temporary directory it removes on the way out.

import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WRAPPER_DIR = path.join(ROOT, 'npm', 'pristine');

const { BINARY, packageForHost, hostLibc } = require(path.join(WRAPPER_DIR, 'lib', 'platform.cjs'));
const { version } = require(path.join(WRAPPER_DIR, 'package.json'));

function run(command, args, options = {}) {
  return execFileSync(command, args, { encoding: 'utf8', stdio: 'pipe', ...options });
}

function check(condition, message) {
  if (!condition) {
    console.error(`FAIL  ${message}`);
    process.exitCode = 1;
    return false;
  }
  console.log(`ok    ${message}`);
  return true;
}

/** `npm pack`, returning the tarball path and the list of files it holds. */
function pack(directory, destination) {
  const [result] = JSON.parse(
    run('npm', ['pack', directory, '--json', '--pack-destination', destination], { cwd: ROOT }),
  );
  return {
    tarball: path.join(destination, result.filename),
    files: result.files.map((entry) => entry.path),
  };
}

const target = packageForHost({ platform: process.platform, arch: process.arch, libc: hostLibc(process) });
const platformDir = path.join(ROOT, 'npm', target.slice('@agentender/'.length));
console.log(`Verifying ${target} at ${version} on ${process.platform}-${process.arch}\n`);

console.log('Building the release binary...');
run('cargo', ['build', '--release', '-p', 'pristine-cli'], { cwd: ROOT, stdio: 'inherit' });

// What the release job does for every target; here, only for the one we can run.
const staged = path.join(platformDir, BINARY);
copyFileSync(path.join(ROOT, 'target', 'release', BINARY), staged);
chmodSync(staged, 0o755);

const scratch = mkdtempSync(path.join(tmpdir(), 'pristine-verify-'));
try {
  const platformPackage = pack(platformDir, scratch);
  const wrapperPackage = pack(WRAPPER_DIR, scratch);

  // A `files` entry naming a file that does not exist is skipped in silence, so an empty
  // platform package is a shape a release can reach. Catch it at the tarball.
  check(platformPackage.files.includes(BINARY), `the platform tarball carries the ${BINARY} binary`);
  check(
    wrapperPackage.files.some((file) => file === 'bin/pristine.cjs') &&
      wrapperPackage.files.some((file) => file.startsWith('lib/')),
    'the wrapper tarball carries the shim and its lib',
  );
  check(
    !wrapperPackage.files.some((file) => file.startsWith('test/')),
    'the wrapper tarball leaves the tests behind',
  );

  // The override pins the optional dependency to the tarball. Without it npm resolves the
  // wrapper's pin from the registry, where this version does not exist yet, and an optional
  // dependency that fails to resolve is skipped in silence rather than reported.
  const consumer = path.join(scratch, 'consumer');
  mkdirSync(consumer);
  writeFileSync(
    path.join(consumer, 'package.json'),
    `${JSON.stringify(
      {
        name: 'pristine-install-check',
        version: '0.0.0',
        private: true,
        dependencies: { '@agentender/pristine': `file:${wrapperPackage.tarball}` },
        overrides: { [target]: `file:${platformPackage.tarball}` },
      },
      null,
      2,
    )}\n`,
  );

  console.log('\nInstalling the tarballs into a scratch project...');
  run('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: consumer, stdio: 'inherit' });

  const installed = path.join(consumer, 'node_modules', '.bin', 'pristine');
  console.log('');

  // The lockstep property, observed rather than read: the binary npm hands you reports the
  // version the wrapper was published at.
  const reported = run(installed, ['--version'], { cwd: consumer }).trim();
  check(reported === `pristine ${version}`, `the installed command reports "pristine ${version}" (got "${reported}")`);

  // The shim sits between the shell and the binary's exit status, and a wrapper that swallows
  // a failure into a zero would break every script that uses it.
  let status = 0;
  try {
    run(installed, ['--no-such-flag'], { cwd: consumer });
  } catch (error) {
    status = error.status;
  }
  check(status !== 0, `a failing run keeps its non-zero exit status (got ${status})`);
} finally {
  rmSync(scratch, { recursive: true, force: true });
  rmSync(staged, { force: true });
}

console.log(process.exitCode ? '\nVerification FAILED.' : '\nVerification passed.');
