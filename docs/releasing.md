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

Nx reads the conventional-commit subjects since the last tag, picks the bump, writes `Cargo.toml`
and all five npm manifests, rewrites the wrapper's `optionalDependencies` pins, regenerates
`Cargo.lock`, writes `CHANGELOG.md`, commits, and tags `vX.Y.Z`.

Run the dry run first and mean it. A full `nx release` does not stop at the commit and tag: it
**pushes to `origin` and creates a GitHub Release**, with no confirmation step between deciding and
publishing.

To exercise the config without any git or GitHub side effects, run the version step on its own.
`npx nx release version --dry-run` writes nothing at all, and `npx nx release version` writes the
manifests and `Cargo.lock` but does not commit, tag, or push.

**`nx release` versions; it does not publish.** Pushing the tag is the whole handoff. Everything
downstream of it is `.github/workflows/release.yml`, described under "What the tag sets off". Do not
run `nx release publish` — see "Why publishing is not `nx release publish`" for what it would
silently do.

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

## What the tag sets off

`.github/workflows/release.yml` runs on any `v*` tag, and `workflow_dispatch` re-runs it against a
tag that already exists. Four jobs:

1. **`build`**, one native runner per target. Cross-compiles are deliberately absent: the job runs
   the binary it just produced to check `pristine --version` against the tag, which is the only step
   that can tell a correctly built binary from a mistagged one. It also stages that binary into its
   npm platform package, packs it, and refuses to go on unless `package/pristine` is in the tarball.
2. **`publish`**, which aggregates the checksums into `SHA256SUMS`, keyless-signs it with cosign,
   generates the Homebrew formula against those checksums, creates the GitHub Release, and pushes the
   formula into the tap.
3. **`publish-crate`**, gated on the `crates-io` environment, minting a short-lived crates.io token
   from the job's OIDC identity.
4. **`publish-npm`**, gated on the `npm` environment, publishing the exact tarballs `build` proved.

Platform packages publish before the wrapper. The wrapper's `optionalDependencies` pin them
exactly, so a wrapper that lands first is installable and broken until they catch up.

A pre-release tag (`v0.1.0-rc.1`) runs `build` and `publish` and skips all three publishing paths:
the tap, crates.io and npm. That is what makes a rehearsal cheap — it exercises the whole binary
pipeline without spending a version anywhere or moving what `brew install` resolves to.

### Homebrew

The tap is [AgentEnder/homebrew-pristine](https://github.com/AgentEnder/homebrew-pristine), and
`brew install AgentEnder/pristine/pristine` is the install string. The formula is `pristine` even
though the crate is `pristine-cli`; confining that collision to one channel is the entire point of
the rename, and `dist_channels.rs` fails if the crate name leaks into the formula.

**There is no committed `pristine.rb`.** `dist/homebrew/gen-formula.sh` is the only copy, and the
release job runs it against the `SHA256SUMS` it just built. A checked-in formula would put the
version in two places bumped by different things — `nx release` moves `Cargo.toml` and knows nothing
about Homebrew — so it would either fail CI on every release or quietly point `brew install` at the
previous release's tarballs. Run the generator by hand to see what it emits:

```sh
bash dist/homebrew/gen-formula.sh --version 0.1.0
```

Without `--checksums` every digest is a 64-zero sentinel. With one, a target missing from the
manifest is a hard failure rather than a sentinel, because a formula carrying zeros is one Homebrew
refuses to install from.

The formula reaches the tap as a push over an SSH deploy key (`TAP_DEPLOY_KEY`), not a token. A
deploy key is scoped to one repository by construction rather than by a setting somebody has to get
right, and it does not expire, so the tap cannot silently stop updating the way a lapsed PAT would.
The cost is that a deploy key authenticates git transport and nothing else, so `repository_dispatch`
is not available and the tap cannot come and fetch.

**The host key is pinned, not scanned.** `dist/homebrew/github_known_hosts` holds GitHub's published
host keys and the push runs with `StrictHostKeyChecking=yes` against it. The obvious
`ssh-keyscan github.com >> known_hosts` authenticates nothing — it learns the key from the very
connection it is meant to check, so whoever answers the scan becomes the trusted host and receives
the formula and the deploy key's authentication attempt, while the job reports a clean push. If
GitHub rotates a key the push fails with `Host key verification failed`; refresh the file with
`gh api meta --jq '.ssh_keys[]'` and re-check the fingerprints in its header against
[GitHub's docs](https://docs.github.com/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints).

**When a push does not land, there is no repair workflow in the tap, on purpose.** One would need a
cross-repository credential: the tap's own `GITHUB_TOKEN` is scoped to the tap and cannot read a
private AgentEnder/pristine's release assets, so a workflow there would fail with a 404 that reads
like the tag does not exist. Two paths work with what already exists:

```sh
# From ~/repos/homebrew-pristine, with the access you already have.
gh release download v0.1.0 --repo AgentEnder/pristine \
  --pattern pristine.rb --output Formula/pristine.rb --clobber
git commit -am "pristine 0.1.0" && git push
```

or re-run this workflow with `workflow_dispatch` against the tag, which regenerates the formula from
that tag's checksums and pushes it again. A re-run against a version already on crates.io or npm will
show those two publish jobs failing; the tap is updated by an earlier job and is unaffected.

### Why publishing is not `nx release publish`

The original design had `nx release publish` covering both registries. It does not work here, and
both reasons were found by wiring it up rather than by reading:

- **It would silently skip the crate.** `nx release publish` only runs projects that have an
  `nx-release-publish` target. `@monodon/rust` attaches that through its inference plugin, and this
  workspace registers no plugins — the crate's `versionActions` works because it is a direct module
  path, not because the plugin is loaded. `nx show project pristine` lists no such target, so the
  command would publish the four npm packages, report success, and ship nothing to crates.io.
- **`@monodon/rust:release-publish` runs `cargo publish --allow-dirty` and never passes `--locked`.**
  Cargo is then free to resolve dependency versions newer than the lockfile CI verified, on the one
  step in this pipeline that cannot be undone.

Publishing from the workflow also buys two things the executor has no place for: each registry's
credential lives in its own job, and there is somewhere to put the assertion between `npm pack` and
`npm publish` that a platform package cannot ship without its binary.

### Credentials the workflow needs

`TAP_DEPLOY_KEY` is set, and the matching read-write deploy key is on the tap. Two remain, and both
need a human at a browser:

- **`NPM_TOKEN`**, a granular npm access token with publish rights to the `@agentender` scope, set as
  a repository secret. Without it `publish-npm` fails with a 404 that reads like the package does not
  exist.
- **crates.io Trusted Publishing** for `pristine-cli`, configured against this repository and the
  `release.yml` workflow. `crates-io-auth-action` exchanges the job's OIDC identity for a short-lived
  token, so there is no `CARGO_REGISTRY_TOKEN` to store — but the trust has to be declared on
  crates.io first.

The `crates-io` and `npm` environments exist but carry no protection rules. Adding required reviewers
to them is what turns the job gating into an actual approval step.

**npm provenance is off, and should be turned on when the repository goes public.** npm only
generates provenance for packages built from a public repository; adding `--provenance` while
pristine is private fails the publish outright.

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

### `npm pack npm/pristine` does not pack `npm/pristine`

`npm/pristine` is a valid GitHub shorthand spec — owner `npm`, repo `pristine` — and npm resolves it
as one, dying on `ls-remote ssh://git@github.com/npm/pristine.git: Repository not found`. Every npm
path in this repository begins `npm/`, so every one of them is ambiguous, and the `./` that
disambiguates is exactly the character a later edit drops as noise. Write `./npm/pristine`.

`tools/verify-npm-install.mjs` sidesteps this by passing absolute paths;
`npm_pack_is_never_handed_an_owner_slash_repo_shorthand` in `dist_channels.rs` is what keeps the
workflow from reintroducing it, because there the failure lands mid-release with the binaries
already built.

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

## Verifying the channels

`packages/pristine/tests/dist_channels.rs` runs in `cargo test` and is the only thing holding the
four channels in step. The same target list is restated in the release matrix, the formula
generator, `npm/pristine/lib/platform.cjs` and the `npm/pristine-*` directories, and nothing
type-checks a shell script against a YAML matrix. When they drift the failure surfaces at a
stranger's install time: a 404 tarball, an `EBADPLATFORM`, or a `pristine: command not found` from a
platform package that shipped a manifest and no binary.

It runs the formula generator for real rather than reading it, including the path where a target is
missing from the checksum manifest, and it fails the build if a third-party action in any workflow
is pinned to a tag instead of a commit. Adding a target means adding a `TARGETS` entry, a directory,
and a matrix row — and the tests name whichever of the three you forgot.
