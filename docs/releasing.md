# Releasing pristine

One `nx release` versions the crate and every npm package together: one version, one changelog, one
tag. The crate publishes to crates.io as `pristine-cli`; the npm side is the `@agentender/pristine`
wrapper plus one package per platform.

## How the config works

Everything lives in `nx.json` under `release`, plus one override in `packages/pristine/project.json`.

There is **one release group holding every project**, not one group per ecosystem:

```jsonc
"groups": { "pristine": { "projects": ["*"] } }
```

Two things about that are easy to get wrong.

`projectsRelationship: "fixed"` is a **within-group** relationship. Splitting the crate and the npm
packages into two groups does not keep them in lockstep — it guarantees they drift, and it does so
silently. A crate-only `feat:` bumps the crate and leaves the npm packages behind, which is the exact
failure the single version exists to prevent.

The group has to be declared even though it selects everything. Nx's implicit default group only picks
up `lib` projects that have a non-private `package.json` in their own root, so a Rust crate never
lands in it. Omit `groups` and Nx releases the npm packages alone, without complaint.

The two ecosystems are bridged by `versionActions`, which is settable per project. The group inherits
the `@nx/js` default, and the crate overrides it for itself:

```jsonc
// packages/pristine/project.json
"release": { "version": { "versionActions": "@monodon/rust/src/release/version-actions" } }
```

Note the nesting: it is `release.version.versionActions`, **not** `release.versionActions`. The
shallower spelling is not rejected — it is ignored. The crate then falls back to Nx's
`NOOP_VERSION_ACTIONS`, and the release runs to completion having versioned every npm package while
leaving `Cargo.toml` untouched.

## The first release is different

`nx release` resolves the current version from the `v{version}` tags, so before one exists it fails
with `No git tags matching pattern "v{version}" were found` and does nothing. The bootstrap names its
version explicitly:

```sh
npx nx release 0.1.0 --first-release --dry-run
npx nx release 0.1.0 --first-release
```

`--first-release` falls back to the manifest version instead of the missing tag, and the explicit
specifier bypasses conventional commits for that run. Afterwards `v0.1.0` exists and every later
release is a plain `npx nx release`.

Do not reach for `version.fallbackCurrentVersionResolver: "disk"` to avoid the flag. It makes the
bootstrap work without `--first-release`, but it also makes a *later* release quietly fall back to the
manifests when the tag it expects is missing, republishing a version that already shipped. Failing
loudly on a missing tag is the behaviour worth keeping.

## Run the dry run first, always

A full `nx release` does not stop at versioning. It commits, tags, **pushes to `origin`, and creates a
GitHub Release**. There is no confirmation step. `--dry-run` prints the whole plan and changes
nothing, so it is the default way to look at a release.

To version without any of the git or GitHub side effects — which is what you want when checking the
config — run the version step alone:

```sh
npx nx release version --dry-run   # no writes at all
npx nx release version             # writes manifests and Cargo.lock, no commit/tag/push
```

## Things that bite

**A `feat:` is a patch bump while the version is 0.x.** Nx shifts bump types down below 1.0 by the
usual convention: breaking becomes minor, feat becomes patch. So `feat:` on 0.1.0 gives 0.1.1, not
0.2.0. The log reports the pre-shift specifier ("Resolved the specifier as minor") and then the
shifted result, which reads like a contradiction until you know about the rule.

**Commit scopes are matched against project names.** A scope that matches no project in the release
group does not get attributed by file path — it is dropped. `feat(cli): …` touching the crate resolves
to a *patch*, while a plain `feat: …` touching the same file resolves to a *minor*. Either leave the
scope off or use a real project name (`pristine`, `npm-pristine`, …).

**The crate's version must be a literal.** `version.workspace = true` in
`packages/pristine/Cargo.toml` does not work: `@monodon/rust` requires `[package].version` to be a
plain string, and inheriting it makes the crate skip versioning while the npm packages bump — the same
split as the two-group mistake, just quieter. The crate carries its own `version = "…"` and the root
`[workspace.package]` deliberately has no `version` key, so there is only one source of truth.

**Versioning rewrites `Cargo.toml` and drops its comments.** The plugin round-trips the manifest
through a TOML parser that reserializes every string with single quotes, adds a leading blank line,
and **discards every comment in the file**. Any comment in `packages/pristine/Cargo.toml` is gone
after the first real release. Comments that need to survive belong in the root `Cargo.toml`, which is
not rewritten, or in this document.

**Platform dependency specs must be exact pins, never carets.**
`preserveMatchingDependencyRanges` defaults to `true`, so `"^0.1.0"` still matches 0.1.1 and is left
alone, while `"0.1.0"` is rewritten. A caret range on a platform package lets npm resolve a binary
from a different release than the wrapper — the exact failure the shared version exists to prevent.

**`--dry-run` cannot prove the `Cargo.lock` step.** `@monodon/rust`'s `afterAllProjectsVersioned` hook
returns early under `dryRun`, so the lockfile only moves on a real run. It shells out to
`cargo update --workspace`, so cargo must be on `PATH` wherever the release runs, and it **swallows
the failure if it is not**, leaving the lockfile silently stale while everything else reports success.

**The npm packages are not workspace members.** They live under `npm/`, outside the `packages/*` glob
in `pnpm-workspace.yaml`. A platform package declares `os` and `cpu`, so a package manager refuses to
link it on any host it does not match, which breaks `install` for everyone. They are registered as Nx
projects with a `project.json` instead, which costs nothing because they have no dependencies.

## Verifying a change to the release config

A crate-only conventional commit is the case worth testing, because it is the one that silently split
under the old two-group design:

```sh
git tag v0.1.0                                    # baseline, if not already tagged
git commit -m "feat: …"                           # touching only packages/pristine/
npx nx release version                            # real run: dry-run cannot move Cargo.lock
```

All five artifacts must land on the same version: `packages/pristine/Cargo.toml`, `Cargo.lock`, and
the three `package.json` files under `npm/` — with the wrapper's `optionalDependencies` pins rewritten
to match. If the crate stayed behind, check the `versionActions` nesting first.
