'use strict';

// Which prebuilt binary this host should run, and where it lives once npm has installed it.
//
// This file is the single source of truth for the target list. `npm/pristine/package.json`'s
// `optionalDependencies` and the `npm/pristine-*` directories have to agree with it, and
// `test/manifests.test.cjs` is what makes sure they do.

const path = require('node:path');

/** The name of the binary inside every platform package. */
const BINARY = 'pristine';

/**
 * One entry per published platform package.
 *
 * `libc` is only set where it narrows the match. Every linux build links against glibc, so a
 * musl host matches nothing here on purpose — see `packageForHost`.
 *
 * Windows is absent because nothing builds or tests a Windows binary yet. The crate compiles
 * for `x86_64-pc-windows-msvc`, so the gap is CI coverage rather than the source; adding the
 * package before a release job produces that binary would publish a promise nothing keeps.
 */
const TARGETS = [
  { platform: 'darwin', arch: 'arm64', package: '@agentender/pristine-darwin-arm64' },
  { platform: 'darwin', arch: 'x64', package: '@agentender/pristine-darwin-x64' },
  { platform: 'linux', arch: 'arm64', libc: 'glibc', package: '@agentender/pristine-linux-arm64-gnu' },
  { platform: 'linux', arch: 'x64', libc: 'glibc', package: '@agentender/pristine-linux-x64-gnu' },
];

/**
 * Which C library this host runs, or `undefined` where the question does not apply.
 *
 * Node does not report this directly. `glibcVersionRuntime` appears in the diagnostic report
 * header only when the runtime is linked against glibc, and its absence on linux means musl.
 * The report is not cheap to build, so it is only asked for on linux, where the answer matters.
 */
function hostLibc(host) {
  if (host.platform !== 'linux') {
    return undefined;
  }
  const report = host.report?.getReport?.();
  return report?.header?.glibcVersionRuntime ? 'glibc' : 'musl';
}

/**
 * The platform package for a host, or a throw naming the host that has no binary.
 *
 * Refusing a near-match is deliberate. Handing a musl host the gnu build would fail later, from
 * the dynamic linker, with a message about a file the user never asked for.
 */
function packageForHost({ platform, arch, libc }) {
  const match = TARGETS.find(
    (target) =>
      target.platform === platform &&
      target.arch === arch &&
      (target.libc === undefined || target.libc === libc),
  );
  if (match) {
    return match.package;
  }
  const host = [platform, arch, platform === 'linux' ? libc : undefined].filter(Boolean).join('-');
  throw new Error(
    `pristine does not ship a prebuilt binary for ${host}.\n` +
      `Prebuilt targets: ${TARGETS.map((target) => `${target.platform}-${target.arch}`).join(', ')}.\n` +
      'Build from source instead: cargo install pristine-cli',
  );
}

/**
 * The binary inside an installed platform package.
 *
 * Resolution goes through the package's manifest rather than the binary itself so that a
 * missing package fails here, with an explanation, rather than as an ENOENT from `spawn`.
 * `resolve` is injected so the failure path is testable.
 */
function binaryPath(packageName, resolve = require.resolve) {
  let manifest;
  try {
    manifest = resolve(`${packageName}/package.json`);
  } catch {
    throw new Error(
      `pristine is installed but ${packageName}, which holds the binary for this platform, is not.\n` +
        'That package is an optional dependency, so it is skipped by --no-optional and by an\n' +
        'install whose lockfile was resolved on a different platform.\n' +
        `Reinstall pristine with optional dependencies enabled, or install ${packageName} directly.`,
    );
  }
  return path.join(path.dirname(manifest), BINARY);
}

/**
 * The binary this host should run. Throws, with a reason, when there is not one.
 *
 * The three properties are read out by name rather than spread: `host` is `process` in the shim,
 * and copying the whole of it on every invocation to reach three values would be silly.
 */
function resolveBinary(host = process, resolve = require.resolve) {
  const target = packageForHost({
    platform: host.platform,
    arch: host.arch,
    libc: hostLibc(host),
  });
  return binaryPath(target, resolve);
}

module.exports = { BINARY, TARGETS, binaryPath, hostLibc, packageForHost, resolveBinary };
