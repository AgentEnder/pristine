# Releasing pristine

One `nx release` versions the crate and the npm packages together: one tag, one changelog, one
version number across both registries. The npm wrapper exists to hand you the binary the crate
built, so a release where the two disagree is a broken release.

## What ships

| Nx project                     | Path                           | Published as                           |
| ------------------------------ | ------------------------------ | -------------------------------------- |
| `pristine`                     | `packages/pristine`            | `pristine-cli` on crates.io            |
| `npm-pristine`                 | `npm/pristine`                 | `@agentender/pristine`                 |
| `npm-pristine-darwin-arm64`    | `npm/pristine-darwin-arm64`    | `@agentender/pristine-darwin-arm64`    |
| `npm-pristine-darwin-x64`      | `npm/pristine-darwin-x64`      | `@agentender/pristine-darwin-x64`      |
| `npm-pristine-linux-arm64-gnu` | `npm/pristine-linux-arm64-gnu` | `@agentender/pristine-linux-arm64-gnu` |
| `npm-pristine-linux-x64-gnu`   | `npm/pristine-linux-x64-gnu`   | `@agentender/pristine-linux-x64-gnu`   |

## Cutting a release

Once `v0.1.0` exists, every release is:

```sh
npx nx release --dry-run
npx nx release
```

Nx reads the conventional-commit subjects since the last tag, picks the bump, writes all four
manifests, rewrites the wrapper's `optionalDependencies` pins, regenerates `Cargo.lock`, writes
`CHANGELOG.md`, commits, and tags `vX.Y.Z`.

Run the dry run first and mean it. A full `nx release` does not stop at the commit and tag: it
**pushes to `origin` and creates a GitHub Release**, with no confirmation step between deciding and
publishing.

To exercise the config without any git or GitHub side effects, run the version step on its own.
`npx nx release version --dry-run` writes nothing at all, and `npx nx release version` writes the
manifests and `Cargo.lock` but does not commit, tag, or push.

Publishing is a separate step, `npx nx release publish`. `@monodon/rust` gives the crate an
`nx-release-publish` target for crates.io and `@nx/js` gives each npm project one for npm.

### The first release

`nx release` resolves the current version from the `v{version}` tags. Until one exists it fails with
`No git tags matching pattern "v{version}" were found` and does nothing. The bootstrap has to name
its version:

```sh
npx nx release 0.1.0 --first-release --dry-run
npx nx release 0.1.0 --first-release
```

`--first-release` falls back to the manifest version instead of the missing tag. The explicit
specifier sets the version, bypassing conventional commits for that run only. Afterwards `v0.1.0`
exists and the plain form works.

Do not replace the flag with `version.fallbackCurrentVersionResolver: "disk"`. It makes the bootstrap
work without `--first-release`, but it also makes a later release fall back to the manifests when the
tag it expects is missing, republishing a version that already shipped. A missing tag should fail
loudly.

## The npm wrapper

`@agentender/pristine` ships no code of its own beyond a shim. Its `optionalDependencies` name one
platform package per prebuilt target, each declaring the `os` and `cpu` it is for, so npm resolves
exactly one and skips the rest. The wrapper's `bin` runs `bin/pristine.cjs`, which asks
`lib/platform.cjs` which package this host wants, resolves that package's manifest, and executes the
`pristine` binary sitting beside it.

Nothing is downloaded at install time and there is no build step, which is the point: the install
works offline, behind a proxy, and where lifecycle scripts are disabled. `test/manifests.test.cjs`
fails if a lifecycle script appears in any of the five manifests.

**`npm/pristine/lib/platform.cjs` is the target list.** `TARGETS` there is the single source of
truth; the wrapper's `optionalDependencies` and the `npm/pristine-*` directories are checked against
it by `npm-pristine:test`. Adding a target means adding an entry, a directory, and a build in the
release job — and the tests name whichever of the three you forgot.

### The binaries are not in git

A platform package is committed with its manifest and no binary. `.gitignore` covers
`/npm/pristine-*/pristine`, and something has to put the file there before `npm pack` runs: the
release job for every target, or `nx run npm-pristine:verify-pack` for the host you are sitting at.

This is the sharpest edge in the whole npm half. **A `files` entry naming a file that does not exist
is skipped in silence** — no warning, no non-zero exit — so a release job whose cross-compile step
failed publishes a platform package containing nothing but a manifest, and the failure surfaces as a
user's `pristine: command not found`. Anything that packs these directories has to assert the binary
is in the tarball first. The verification below does.

### Windows

There is no Windows package. The crate does compile for `x86_64-pc-windows-msvc` — checked, not
assumed — so the gap is that nothing builds or tests a Windows binary: CI runs on ubuntu and macos
only, and the deleter's safety model is written against `openat`/`O_NOFOLLOW` semantics that no test
exercises on Windows. Publishing the package before that changes would promise a platform nothing
verifies. Adding it later is an entry in `TARGETS`, a directory, and a matrix row.

## How the config holds the two ecosystems together

`nx.json` declares a single release group covering every project:

```jsonc
"release": {
  "projectsRelationship": "fixed",
  "releaseTag": { "pattern": "v{version}" },
  "version": { "conventionalCommits": true },
  "changelog": {
    "workspaceChangelog": { "createRelease": "github" },
    "projectChangelogs": false
  },
  "groups": { "pristine": { "projects": ["*"] } }
}
```

and `packages/pristine/project.json` overrides the version actions for itself alone:

```jsonc
"release": { "version": { "versionActions": "@monodon/rust/src/release/version-actions" } }
```

Three things about that shape are load-bearing.

**One group, not two.** `projectsRelationship: "fixed"` is a within-group relationship. Splitting the
crate and the npm packages into separate groups does not keep them in lockstep, it does the opposite:
a crate-only `feat:` bumps `pristine-cli` and leaves the npm packages behind. Because
`versionActions` is settable per project, one group can span both ecosystems. The group inherits the
`@nx/js` default and the crate overrides it.

**The group must be declared** even though it covers the whole workspace. Nx's implicit default group
only takes `lib` projects that have a non-private `package.json` in their own root, so a Rust crate
is never in it. Omit `groups` and the release silently covers the npm packages alone.

**The override key is `release.version.versionActions`,** nested under `version`. A bare
`release.versionActions` in `project.json` is ignored, and the crate falls back to the `@nx/js`
actions, which cannot read a `Cargo.toml`.

## Things that bite

### The crate's version must be a literal

`packages/pristine/Cargo.toml` carries `version = "0.1.0"` rather than `version.workspace = true`,
and the root `Cargo.toml` has no `[workspace.package] version` at all. This is not a style choice.
`@monodon/rust` reads `[package].version` and requires a plain string; an inherited version parses to
`{ workspace: true }`, an object. Its writer also assigns straight to `[package].version` in the
member manifest, which is not where an inherited version lives. Restore the inheritance and the crate
silently skips versioning while the npm packages bump.

### A `feat:` is a patch bump below 1.0

Nx shifts bump types down while the major is 0, by the usual convention: breaking becomes minor, a
feature becomes patch. So `feat:` against 0.1.0 gives 0.1.1, not 0.2.0. The log reports the
pre-shift specifier and then the shifted result, which reads as a contradiction until you know the
rule is there:

```
📄 Resolved the specifier as "minor" using git history and the conventional commits standard
❓ Applied semver relative bump "minor", ... to get new version 0.1.1
```

### Commit scopes are matched against project names

A scope is not decoration. Nx matches it against the project names in the release group, and a scope
matching none of them is dropped rather than falling back to attribution by file path. `feat(cli):`
touching the crate resolves to a *patch*; a plain `feat:` touching the same file resolves to a
*minor*. Leave the scope off, or use a real project name (`pristine`, `npm-pristine`, and so on).

### Platform pins must be exact, never carets

`preserveMatchingDependencyRanges` defaults to `true` in Nx 22 and later, so `"^0.1.0"` still matches
`0.1.1` and is left untouched, while `"0.1.0"` is rewritten. A caret range on a platform package lets
npm resolve a binary from a different release than the wrapper, which is the failure the lockstep
version exists to prevent. Keep `optionalDependencies` in `npm/pristine/package.json` bare.

### `--dry-run` cannot prove the `Cargo.lock` step

`@monodon/rust`'s `afterAllProjectsVersioned` hook returns early under `dryRun`, so the lockfile only
moves on a real run. The hook shells out to `cargo update --workspace`, so cargo must be on `PATH`
wherever the release runs. If it is not, the hook catches the failure and reports no changed files,
leaving `Cargo.lock` stale and the release commit failing CI. When a release runs somewhere new,
check that `Cargo.lock` is in the commit.

### Versioning reformats `Cargo.toml`

The plugin round-trips the manifest through `@ltd/j-toml`, which reserializes strings with single
quotes, adds a leading blank line, and **discards every comment in the file**. Cargo does not care,
but the release commit touches more lines than it changed. Do not try to revert the noise by hand;
the next release reintroduces it.

The comment loss is the part worth planning around: the first real release deletes the explanatory
comments at the top of `packages/pristine/Cargo.toml`, including the one recording why the crate is
named `pristine-cli` and why its version is a literal. Anything that must survive belongs in the root
`Cargo.toml`, which is not rewritten, or in this file.

### The npm packages are not workspace members

`pnpm-workspace.yaml` lists `packages/*` only. A platform package declares `os` and `cpu`, so a
package manager refuses to link it on any host it does not match and the install dies with
`EBADPLATFORM` everywhere. The npm packages are registered as Nx projects with `project.json`
instead, which costs nothing because they have no dependencies to install. Adding `npm/*` to
`pnpm-workspace.yaml` breaks `pnpm install --frozen-lockfile` in CI.

### Nx 22 moved two defaults

`updateDependents` now defaults to `"always"`, and `releaseTagPattern` moved under
`releaseTag.pattern`. A `releaseTagPattern` at the top of the `release` block is ignored, and the tag
falls back to Nx's default of `v{version}` for the whole workspace, which happens to be the same
string. Guides written against Nx 21 or earlier will disagree with this file.

## Verifying a config change

The check that matters is the lockstep one, and it needs a real run rather than a dry run. On a
scratch branch, make a crate-only conventional commit, run `npx nx release version` (the version step
alone, so nothing is committed, tagged, or pushed), and confirm that
`packages/pristine/Cargo.toml`, all five `npm/*/package.json` versions, the wrapper's
`optionalDependencies` pins, and `Cargo.lock` all moved to the same version. Then reset the branch.

## Verifying the wrapper

Manifests can be read and still be wrong. The executable bit surviving `npm pack`, npm honouring
`os` and `cpu`, the shim resolving a package it does not depend on directly, and the binary's own
`--version` agreeing with the wrapper's are all properties of an installed tree.

```sh
pnpm nx run npm-pristine:verify-pack
```

It builds the release binary, stages it into this host's platform package, packs both, installs the
tarballs into a scratch project, and runs the `pristine` npm put on the `PATH`. It covers one
platform — whichever one you are on — which is the honest limit of a local check; the release job
is what covers the rest.

One thing it does that is easy to get wrong by hand: installing the two tarballs side by side is not
enough. npm resolves the wrapper's `optionalDependencies` pin against the registry, where an
unpublished version does not exist, and **an optional dependency that fails to resolve is skipped in
silence**. The scratch project pins the edge to the tarball with an `overrides` entry, so a pass
means the wrapper found a binary rather than that npm quietly declined to install one.
