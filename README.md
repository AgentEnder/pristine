# pristine

A language-agnostic reclaimable-space finder and cleaner.

`du` tells you where the bytes are. It cannot tell you which of them you are allowed to delete.
pristine answers the second question: it finds build artifacts and vendored dependency directories
across every ecosystem on the machine, shows you what each costs, and tells you the command that
regenerates it before you decide.

`node_modules` is one ecosystem's answer to a question every ecosystem answers. The same disk is also
carrying `target/`, `.venv/`, `bin/`, `obj/`, `_build/`, `.gradle/` and `vendor/`, all equally
reclaimable and all invisible to a tool that only knows about npm.

> **Status: early.** The parallel walker, both detection tiers and the deleter work and are tested.
> The rollup tree TUI does not exist yet, so there is nothing to select with: `--delete` means
> everything the scan found. The design is settled and lives outside this repo.

## Using it

```sh
pristine ~/repos                       # list what is reclaimable, and what regenerates each
pristine ~/repos --dry-run             # the plan it would execute, and what it would refuse
pristine ~/repos --delete              # the same plan, then a confirmation that defaults to no
pristine ~/repos --delete --yes --older-than 30d
```

```console
$ pristine ~/repos/pua
  59.1 MiB  .nx                       no known way to regenerate this
         —  dist                      nx reset, then rebuild
         —  node_modules              pnpm install
         —  target                    cargo build

5 directories reclaimable, 59.1 MiB priced, 4 not priced
fallback tier: 1 directory found in 1 work tree above a 10.0 MiB floor
```

A dash is not a zero: nothing looked inside, because a matched directory is never enumerated by
the scan that found it. Pruning at `node_modules` and then walking it to size it would give back
everything the pruning saved, so sizes read `—` until something asks for a breakdown, and a removal
reports the bytes it actually freed. The fallback tier's rows do carry a size, because that tier
cannot claim a directory without walking it.

A scan that could not read everything it was pointed at says `scan incomplete` and exits
non-zero, so a listing that is a lower bound never looks — to a script — like the whole truth.

## How it finds things

Two tiers.

**A curated marker ruleset.** Detection is marker-anchored, never name-anchored, because directory
names collide across ecosystems in ways that matter. `target/` is Rust's build output and also
Maven's. `vendor/` belongs to Go, Composer and Bundler. `build/` is Gradle's output, Dart's output,
and in a CMake project it is ordinary source that must never be touched. So a rule is a pair: a
directory name plus a marker file that has to be present in its parent. `node_modules` next to a
`package.json` is reclaimable. A `build` directory next to nothing in particular is not.

**A gitignore fallback.** Inside a git work tree, a directory that is ignored, contains no tracked
file at any depth, holds no git checkout, and exceeds a size floor (10 MiB, `--min-size`) is
reclaimable by inference even when no rule names it. This is what makes the tool genuinely
language-agnostic rather than agnostic across whichever ecosystems happened to get a rule written.
On one real machine it is what turns up `dist/`, `tmp/`, `artifacts/`, `playwright-report/`,
`.angular/cache` and a downloaded `Godot.app` — none of which any ruleset names.

The last two conditions are the safety properties, and both are guarantees `git clean` enforces:
it will not remove a directory holding a tracked file, and it skips rather than collapses one
holding a checkout. Outside a git work tree the tier is **inert**, and says so rather than
reporting an empty result. With no repository the only signal left would be the directory's name,
and a name is not evidence — guessing from one is how a cleaner deletes somebody's source.

A tier-two hit reports that it does not know how to regenerate what it found. That asymmetry
against tier one is the point: it tells you which deletions are cheap.

## Two modes

- **sweep** across a directory tree, for "my disk is full". Every project underneath, sorted by size.
- **repo** inside a single checkout, git-aware, inheriting `git clean`'s exact semantics for nested
  ignore files, negations and `info/exclude`, plus the guarantee that a directory holding a tracked
  file is never removed.

Both share one walker and one deleter.

## Safety

Deletion is by `unlink`, not by moving to the platform trash. Trash is a move, and across filesystems
a copy, which is exactly the wrong thing to do to a 40 GB tree. The checks below carry the weight
instead, and every one of them is a test rather than a promise.

- Every target's path is resolved — `..` and symlinked ancestors and all — and proved to be under the
  scan root before any unlink. The scan root itself is never a target.
- Nothing is removed by name. The scan root is opened once and every entry beneath it is reached by
  `openat` from an already-open parent with `O_NOFOLLOW`, then removed by `unlinkat` against that
  same descriptor. Re-pointing a directory mid-run — even while the removal is inside it — can make
  the removal fail and say so, but it cannot redirect one out of the root.
- The scan root is the one name that still has to be resolved, so it is checked twice: its final
  component is opened without following a symlink, and the descriptor is then matched against the
  device and inode recorded when the plan was built. A root renamed away and replaced — even by an
  ordinary directory on the same disk, laid out to match — is reported rather than swept.
- Symlinks are never followed out of the root. A symlinked target is unlinked as a link, and so is
  every link found inside one.
- A filesystem boundary is not crossed unless `--one-file-system=false`.
- A directory holding a git checkout is refused and reported rather than swept up, at any depth. It
  stops that subtree, and everything above the refusal is left standing.
- `--older-than <duration>` keeps anything touched recently, because a `node_modules` you used this
  morning is not reclaimable in any useful sense. Off by default, and worth turning on.
- `--dry-run` prints the plan and deletes nothing. The final confirmation defaults to no, and so does
  end of input — a script consents with `--yes` or not at all.
- Failures never abort the batch; they are collected, reported, and set a non-zero exit. So does a
  scan that could not read everything it was pointed at.

## Install

Not yet published. When it is:

```sh
brew install pristine
npx @agentender/pristine
cargo install pristine-cli    # the crate is `pristine-cli`; the binary is `pristine`
```

## Development

[mise](https://mise.jdx.dev) owns the toolchain; `mise install` gets you rust, node and pnpm at the
versions CI uses.

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The same commands are wrapped as Nx targets (`pnpm nx run-many -t fmt-check lint test`), which is how
the npm wrapper will share a graph with the crate later.

The crate is `packages/pristine` and its published name is `pristine-cli`; the binary is `pristine`.

## License

MIT
