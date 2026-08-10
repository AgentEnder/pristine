'use strict';

// The shim's job is to turn "what am I running on" into "which file do I exec", and to say
// something useful when the answer is nothing. Both halves are pure given an injected
// `process` and an injected resolver, so both are tested here without spawning anything.

const assert = require('node:assert/strict');
const path = require('node:path');
const { test } = require('node:test');

const { TARGETS, binaryPath, hostLibc, packageForHost, resolveBinary } = require('../lib/platform.cjs');

test('darwin arm64 resolves to the darwin-arm64 package', () => {
  assert.equal(
    packageForHost({ platform: 'darwin', arch: 'arm64' }),
    '@agentender/pristine-darwin-arm64',
  );
});

test('darwin x64 resolves to the darwin-x64 package', () => {
  assert.equal(
    packageForHost({ platform: 'darwin', arch: 'x64' }),
    '@agentender/pristine-darwin-x64',
  );
});

test('linux x64 on glibc resolves to the linux-x64-gnu package', () => {
  assert.equal(
    packageForHost({ platform: 'linux', arch: 'x64', libc: 'glibc' }),
    '@agentender/pristine-linux-x64-gnu',
  );
});

test('linux arm64 on glibc resolves to the linux-arm64-gnu package', () => {
  assert.equal(
    packageForHost({ platform: 'linux', arch: 'arm64', libc: 'glibc' }),
    '@agentender/pristine-linux-arm64-gnu',
  );
});

// Every published linux package is `-gnu`. A musl host must be told that rather than handed a
// glibc binary that dies with a linker error naming a file it has never heard of.
test('linux on musl is refused rather than handed a gnu binary', () => {
  assert.throws(
    () => packageForHost({ platform: 'linux', arch: 'x64', libc: 'musl' }),
    (error) => {
      assert.match(error.message, /linux-x64-musl/);
      assert.match(error.message, /cargo install pristine-cli/);
      return true;
    },
  );
});

test('an unsupported platform names itself and points at the source build', () => {
  assert.throws(
    () => packageForHost({ platform: 'win32', arch: 'x64' }),
    (error) => {
      assert.match(error.message, /win32-x64/);
      assert.match(error.message, /cargo install pristine-cli/);
      return true;
    },
  );
});

// libc only narrows the linux targets. Passing it on darwin, or omitting it there, must not
// change the answer.
test('libc does not participate in the match off linux', () => {
  assert.equal(
    packageForHost({ platform: 'darwin', arch: 'arm64', libc: 'musl' }),
    '@agentender/pristine-darwin-arm64',
  );
});

test('a glibc runtime version in the report means glibc', () => {
  const host = {
    platform: 'linux',
    report: { getReport: () => ({ header: { glibcVersionRuntime: '2.39' } }) },
  };
  assert.equal(hostLibc(host), 'glibc');
});

test('a linux report with no glibc runtime version means musl', () => {
  const host = { platform: 'linux', report: { getReport: () => ({ header: {} }) } };
  assert.equal(hostLibc(host), 'musl');
});

test('libc is not probed at all off linux', () => {
  const host = {
    platform: 'darwin',
    report: {
      getReport: () => assert.fail('the diagnostic report must not be built off linux'),
    },
  };
  assert.equal(hostLibc(host), undefined);
});

test('the binary sits beside the platform package manifest', () => {
  const resolved = binaryPath('@agentender/pristine-darwin-arm64', () =>
    path.join('/somewhere', 'node_modules', '@agentender', 'pristine-darwin-arm64', 'package.json'),
  );
  assert.equal(
    resolved,
    path.join('/somewhere', 'node_modules', '@agentender', 'pristine-darwin-arm64', 'pristine'),
  );
});

// The classic failure of this packaging pattern: the optional dependency was skipped, so the
// wrapper is present and the binary is not. Guessing is worse than saying so.
test('a platform package that was never installed explains why it might be missing', () => {
  assert.throws(
    () =>
      binaryPath('@agentender/pristine-darwin-arm64', () => {
        const error = new Error("Cannot find module '@agentender/pristine-darwin-arm64'");
        error.code = 'MODULE_NOT_FOUND';
        throw error;
      }),
    (error) => {
      assert.match(error.message, /@agentender\/pristine-darwin-arm64/);
      assert.match(error.message, /--no-optional|optional/);
      return true;
    },
  );
});

// The composition the shim actually calls. It reads only these three properties off the host,
// so a plain object is a complete stand-in for `process` and nothing else about it is copied.
test('a host is taken all the way to a binary path', () => {
  const resolved = resolveBinary(
    { platform: 'linux', arch: 'x64', report: { getReport: () => ({ header: { glibcVersionRuntime: '2.39' } }) } },
    (request) => {
      assert.equal(request, '@agentender/pristine-linux-x64-gnu/package.json');
      return path.join('/n', '@agentender', 'pristine-linux-x64-gnu', 'package.json');
    },
  );
  assert.equal(resolved, path.join('/n', '@agentender', 'pristine-linux-x64-gnu', 'pristine'));
});

test('a host with no prebuilt binary fails before any resolution is attempted', () => {
  assert.throws(
    () =>
      resolveBinary({ platform: 'linux', arch: 'x64', report: { getReport: () => ({ header: {} }) } }, () =>
        assert.fail('resolution must not be attempted for an unsupported host'),
      ),
    /linux-x64-musl/,
  );
});

test('every target is distinct in platform, arch and libc', () => {
  const keys = TARGETS.map((target) => `${target.platform}-${target.arch}-${target.libc ?? ''}`);
  assert.equal(new Set(keys).size, keys.length);
});
