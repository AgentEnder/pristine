#!/usr/bin/env node
'use strict';

// The whole npm wrapper. Find the binary npm installed for this platform, run it, and get out
// of the way of its exit status.
//
// `spawnSync` with inherited stdio rather than a re-exec: pristine reads a typed confirmation
// from stdin before deleting anything, so the child needs the real terminal on all three
// descriptors, not a pipe this process copies between.

const { spawnSync } = require('node:child_process');
const os = require('node:os');

const { resolveBinary } = require('../lib/platform.cjs');

let binary;
try {
  binary = resolveBinary(process);
} catch (error) {
  console.error(error.message);
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`pristine: could not run ${binary}\n${result.error.message}`);
  process.exit(1);
}

// A child killed by a signal has no exit status, and reporting one would tell the shell the
// run ended normally. Re-raise the signal so this process dies the same way the binary did,
// which is what a `^C` mid-scan should look like from the outside.
if (result.signal) {
  process.kill(process.pid, result.signal);
  // Only reached if the signal is ignored here. 128+n is the shell's own convention for it.
  process.exit(128 + (os.constants.signals[result.signal] ?? 0));
}

process.exit(result.status ?? 1);
