# pristine

A language-agnostic reclaimable-space finder and cleaner.

`du` tells you where the bytes are. It cannot tell you which of them you are allowed to delete.
pristine answers the second question: it finds build artifacts and vendored dependency directories
across every ecosystem on the machine, shows you what each costs, and tells you the command that
regenerates it before you decide.

`node_modules` is one ecosystem's answer to a question every ecosystem answers. The same disk is also
carrying `target/`, `.venv/`, `bin/`, `obj/`, `_build/`, `.gradle/` and `vendor/`, all equally
reclaimable and all invisible to a tool that only knows about npm.

> **Status: early.** The walker, the curated ruleset and the deleter work. The rollup tree TUI does
> not exist yet, so `--delete` means everything the scan found rather than a selection. The design is
> settled and lives outside this repo.

## Using it

```sh
pristine ~/repos                       # list what is reclaimable, and what regenerates each
pristine ~/repos --dry-run             # the plan it would execute, and what it would refuse
pristine ~/repos --delete              # the same plan, then a confirmation that defaults to no
pristine ~/repos --delete --yes --older-than 30d
```

A scan does not measure what it claims. Pruning at `node_modules` and then walking it to size it
would give back everything the pruning saved, so sizes read `—` until something asks for a
breakdown; a removal reports the bytes it actually freed.

## How it finds things

Two tiers.

**A curated marker ruleset.** Detection is marker-anchored, never name-anchored, because directory
names collide across ecosystems in ways that matter. `target/` is Rust's build output and also
Maven's. `vendor/` belongs to Go, Composer and Bundler. `build/` is Gradle's output, Dart's output,
and in a CMake project it is ordinary source that must never be touched. So a rule is a pair: a
directory name plus a marker file that has to be present in its parent. `node_modules` next to a
`package.json` is reclaimable. A `build` directory next to nothing in particular is not.

**A gitignore fallback.** Inside a git work tree, a directory that is ignored, contains no tracked
file at any depth, and exceeds a size floor is reclaimable by inference even when no rule names it.
This is what makes the tool genuinely language-agnostic rather than agnostic across whichever
ecosystems happened to get a rule written. The "no tracked files" condition is the safety property,
and it is exactly the guarantee `git clean` enforces.

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
