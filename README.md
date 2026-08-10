# pristine

A language-agnostic reclaimable-space finder and cleaner.

`du` tells you where the bytes are. It cannot tell you which of them you are allowed to delete.
pristine answers the second question: it finds build artifacts and vendored dependency directories
across every ecosystem on the machine, shows you what each costs, and tells you the command that
regenerates it before you decide.

`node_modules` is one ecosystem's answer to a question every ecosystem answers. The same disk is also
carrying `target/`, `.venv/`, `bin/`, `obj/`, `_build/`, `.gradle/` and `vendor/`, all equally
reclaimable and all invisible to a tool that only knows about npm.

> **Status: early.** The parallel walker, both detection tiers, the deleter and both modes work
> and are tested. The rollup tree TUI does not exist yet, so in the sweep there is nothing to
> select with: `--delete` means everything the scan found. The design is settled and lives outside
> this repo.

## Using it

```sh
pristine ~/repos                       # list what is reclaimable, and what regenerates each
pristine ~/repos --dry-run             # the plan it would execute, and what it would refuse
pristine ~/repos --delete              # the same plan, then a confirmation that defaults to no
pristine ~/repos --delete --yes --older-than 30d

pristine repo                          # one checkout: ask what to clean, then clean it
pristine repo --untracked --ignored --dry-run
pristine repo --reset=hard --untracked --ignored --yes
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

**sweep** — a bare `pristine [PATH]` — walks a directory tree for "my disk is full". Every project
underneath, sorted by size. Everything above describes it.

**`pristine repo`** cleans one git checkout, and replaces `git clean -fdx`. It enumerates nothing
itself: `git clean -n -d` lists the untracked files and `git clean -n -d -X` lists the ignored ones,
so nested ignore files, negations, `info/exclude`, your global excludes and the refusal to touch a
nested repository are inherited exactly rather than reimplemented. The two lists are disjoint, which
is why each is a separate choice.

```console
$ pristine repo --untracked --ignored --dry-run
         —  .nx/workspace-data
         —  dist
         —  target

plan: 3 paths, 0 B priced, 3 not priced
excluded: 1 vendored path (--node-modules includes them)
skipped: 1 nested repository git will not clean
  sandboxes/work

dry run: nothing was reset and nothing was removed
```

| Flag | Meaning |
|---|---|
| `--untracked` | remove untracked files |
| `--ignored` | remove ignored files |
| `--reset[=worktree\|hard]` | discard tracked changes; bare `--reset` is `hard` |
| `--node-modules[=BOOL]` | include vendored dependency directories (off) |
| `--env[=BOOL]` | include `*.env*` files (off) |
| `--dry-run` | print the plan, change nothing |
| `--yes`, `-y` | answer the final confirmation |

Reset first, then the removal. `--reset=worktree` is `git restore -- .` and keeps the index;
`--reset=hard` is `git reset --hard HEAD` and does not.

A reset moves the index, and the index is what makes a path untracked — so **the enumeration does
not outlive it**. After the reset the work tree is asked again, and the second answer is narrowed
to what you were shown and confirmed. Both halves matter. Re-asking is what stops a file the reset
made *tracked* from being deleted: `git rm --cached committed.txt` leaves it on disk and out of the
index, so `git clean` offers it, and `--reset=hard` then puts it back. Narrowing is what stops a
directory the reset made git *collapse* from being removed without ever appearing on a plan — which
would take the vendor and env files you were told had been held back. Anything withdrawn that way
is named, and a second run shows it honestly.

With no action flag it asks — reset, untracked, ignored, then vendor and env — and every question
defaults to the answer that changes nothing, so a run with nothing on its standard input does
nothing. With *any* action flag it does not ask, so nothing in CI hangs on a prompt.

`--yes` gates the final confirmation and nothing else. It selects nothing, so `pristine repo --yes`
on its own does nothing, and `pristine repo --ignored` in a script still refuses to delete without
it. It does count as the command line resolving the plan, so it makes the run non-interactive too —
otherwise `--yes` would let you be asked what to clean and then never asked to confirm it, which
turns "I consent to what I asked for" into "I consent to whatever I am about to be asked".

Vendor and env are held back even from a list you did ask for, in both lists rather than only in
the ignored one. `node_modules` costs minutes and a network to get back, and nothing at all
regenerates a `.env` — least of all one that is untracked rather than ignored, which is the copy
git is not even hiding.

**That applies to what an entry hides as well as to what it is.** `git clean` offers a whole
directory whenever everything inside it is removable, so a row is not a description of its own
contents: `docker/` arrives as one line and may hold `docker/.env`. A directory that hides
something you did not ask to remove is held back whole, and named:

```console
$ pristine repo --untracked --ignored --yes
         —  scratch.txt

plan: 1 path, 0 B priced, 1 not priced
held back: 3 paths, because git offered them whole and they hold something you did not ask to remove
  docker  —  holds docker/.env, which is an env file (--env includes it)
  pkg  —  holds pkg/node_modules, which is vendored (--node-modules includes it)
  build  —  holds build/.env, which is an env file (--env includes it)
```

Held back whole rather than cleaned around, because cleaning around it would mean deciding for
ourselves what inside it is removable — the reimplementation of `git clean` this mode exists to
avoid. A directory that cannot be read is held back on the same rule: "I could not look" is not
"there was nothing there".

Both modes share one deleter and the whole of the safety model below.

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
