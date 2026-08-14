//! Guards the public distribution channels against drifting apart.
//!
//! pristine ships through four channels, all cut from one tag, and the same
//! handful of facts is restated in each one's own syntax:
//!
//!   - `.github/workflows/release.yml` builds a set of target triples.
//!   - `dist/homebrew/gen-formula.sh` writes a tarball URL per triple.
//!   - `npm/pristine/lib/platform.cjs` names one npm package per triple.
//!   - `npm/pristine-*/` is a directory per npm package.
//!
//! Nothing type-checks a shell script against a YAML matrix, and nothing checks
//! either against a JavaScript array. When these fall out of lockstep the
//! failure surfaces at a stranger's install time — a 404 tarball, an
//! `EBADPLATFORM`, or a `pristine: command not found` from a platform package
//! that shipped a manifest and no binary. These tests fail the build instead.
//!
//! Two tests here are about supply chain rather than drift: the workflow that
//! holds `contents: write`, `id-token: write` and the tap's deploy key must name
//! the third-party code it runs by immutable commit, and a rehearsal tag must
//! not move what `brew install` resolves to.
//!
//! See brain: `areas/pristine/releasing.md`, and `docs/releasing.md`.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and
// the readers and parsers below sit outside one. A file this suite cannot read is a broken
// checkout, not a condition to handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::process::Command;

/// Repo root, relative to the package directory cargo runs an integration test
/// from. Every artifact guarded here lives outside the crate.
const ROOT: &str = "../..";

/// GitHub's SSH host keys, pinned so the tap push can check what it is talking
/// to. Repo-relative, and named once because both the test and the workflow have
/// to agree on it.
const KNOWN_HOSTS: &str = "dist/homebrew/github_known_hosts";

fn read(rel: &str) -> String {
    let path = format!("{ROOT}/{rel}");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One row of `release.yml`'s build matrix: the triple, the runner image it is
/// built on, and the npm platform package its binary is staged into.
struct BuildTarget {
    triple: String,
    runner: String,
    npm_dir: String,
}

/// Parse the `- target: … / os: … / npm: …` rows out of the release workflow's
/// matrix.
///
/// Deliberately positional rather than a YAML parse: the pairing *is* the thing
/// under test, and a real parser would happily accept a matrix whose rows had
/// drifted apart into three independent lists.
fn build_matrix() -> Vec<BuildTarget> {
    let yml = read(".github/workflows/release.yml");
    let mut targets: Vec<BuildTarget> = Vec::new();
    for line in yml.lines() {
        let line = line.trim();
        if let Some(triple) = line.strip_prefix("- target:") {
            targets.push(BuildTarget {
                triple: triple.trim().to_owned(),
                runner: String::new(),
                npm_dir: String::new(),
            });
            continue;
        }
        // Each belongs to the row opened above it. Attaching to the most recent
        // `- target:` — rather than searching forward — is what makes a row that
        // lost a field a failure here instead of one that silently borrows its
        // neighbour's.
        let (field, value) = if let Some(v) = line.strip_prefix("os:") {
            ("os", v)
        } else if let Some(v) = line.strip_prefix("npm:") {
            ("npm", v)
        } else {
            continue;
        };
        let current = targets
            .last_mut()
            .unwrap_or_else(|| panic!("a matrix `{field}:` must follow a `- target:` row"));
        let slot = if field == "os" {
            &mut current.runner
        } else {
            &mut current.npm_dir
        };
        assert!(
            slot.is_empty(),
            "matrix row `{}` has two `{field}:` values",
            current.triple
        );
        value.trim().clone_into(slot);
    }
    for target in &targets {
        assert!(
            !target.runner.is_empty(),
            "matrix row `{}` has no `os:` runner",
            target.triple
        );
        assert!(
            !target.npm_dir.is_empty(),
            "matrix row `{}` has no `npm:` platform package",
            target.triple
        );
    }
    assert_eq!(
        targets.len(),
        4,
        "expected 4 release targets, got {:?}",
        targets.iter().map(|t| &t.triple).collect::<Vec<_>>()
    );
    targets
}

/// Run `dist/homebrew/gen-formula.sh`, returning whatever it did.
///
/// The generator is the only copy of the formula this repo keeps. There is no
/// committed `pristine.rb` on purpose: `nx release` bumps the version in
/// `Cargo.toml` and the npm manifests and knows nothing about a checked-in
/// Homebrew formula, so a committed one would carry the *previous* version into
/// every release — either failing this suite on every release or, if its version
/// went untested, silently pointing `brew install` at the last release's
/// tarballs. Testing the generator's output removes the second copy rather than
/// trying to keep it in step.
fn run_generator(args: &[&str]) -> std::process::Output {
    // Run from the repo root with a repo-relative script path, which is how
    // `release.yml` invokes it — so a generator that quietly grew a dependency
    // on its working directory fails here rather than in a release.
    Command::new("bash")
        .arg("dist/homebrew/gen-formula.sh")
        .args(args)
        .current_dir(ROOT)
        .output()
        .expect("gen-formula.sh should be runnable with bash")
}

fn generate_formula(args: &[&str]) -> String {
    let out = run_generator(args);
    assert!(
        out.status.success(),
        "gen-formula.sh {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the formula should be UTF-8")
}

/// Write a `SHA256SUMS` manifest listing `targets`, in the layout
/// `sha256sum ./*.tar.gz` produces, and return its path and the digests used.
fn manifest_for(tag: &str, version: &str, targets: &[String]) -> (std::path::PathBuf, Vec<String>) {
    let dir = std::env::temp_dir().join(format!("pristine-dist-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir should be creatable");
    // One repeated hex digit per target, so a lookup that returns the WRONG
    // target's digest is as visible in a failure message as one that returns
    // none at all.
    let mut digests = Vec::with_capacity(targets.len());
    let mut body = String::new();
    for (i, target) in targets.iter().enumerate() {
        let nth = u32::try_from(i).expect("a handful of targets fits a u32");
        let digest = std::char::from_digit(nth + 1, 16)
            .expect("index fits a hex digit")
            .to_string()
            .repeat(64);
        writeln!(body, "{digest}  ./pristine-{version}-{target}.tar.gz")
            .expect("writing to a String cannot fail");
        digests.push(digest);
    }
    let path = dir.join("SHA256SUMS");
    fs::write(&path, body).expect("manifest should be writable");
    (path, digests)
}

/// The formula links a concrete tarball per target; a missing one 404s at
/// `brew install` time on that architecture and nowhere else.
#[test]
fn homebrew_formula_links_every_release_target() {
    let version = env!("CARGO_PKG_VERSION");
    let formula = generate_formula(&["--version", version]);
    for target in build_matrix() {
        let asset = format!("pristine-{version}-{}.tar.gz", target.triple);
        assert!(
            formula.contains(&asset),
            "the generated formula has no URL for {asset}"
        );
    }
}

/// The formula is `pristine` even though the crate is `pristine-cli`.
///
/// The rename exists so the crates.io collision leaks into exactly one install
/// string. Homebrew derives the formula's name from its file name and its class,
/// so a generator that followed the crate would make the tap answer to
/// `brew install AgentEnder/pristine/pristine-cli` — the collision spreading to
/// a second channel it was never meant to reach.
#[test]
fn homebrew_formula_is_named_for_the_binary_not_the_crate() {
    let formula = generate_formula(&["--version", env!("CARGO_PKG_VERSION")]);
    assert!(
        formula.contains("class Pristine < Formula"),
        "the formula class must be `Pristine`, so the tap installs as `pristine`:\n{formula}"
    );
    assert!(
        !formula.contains("pristine-cli"),
        "the crate name leaked into the formula, which only ever names the binary:\n{formula}"
    );
    assert!(
        formula.contains(r#"bin.install "pristine""#),
        "the formula must install the `pristine` binary out of the tarball root:\n{formula}"
    );
}

/// Without a checksum manifest the generator emits an all-zero sentinel per
/// target — the shape the release workflow overwrites with real digests. Assert
/// there are exactly four, all 64 hex characters: a malformed digest (a
/// truncated paste, a leftover placeholder word) makes `brew install` reject
/// the download it just fetched.
#[test]
fn homebrew_formula_emits_one_wellformed_sha_per_target() {
    let formula = generate_formula(&["--version", env!("CARGO_PKG_VERSION")]);
    let digests: Vec<&str> = formula
        .lines()
        .filter_map(|l| l.trim().strip_prefix("sha256 \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    assert_eq!(
        digests.len(),
        4,
        "expected one sha256 per release target, got {digests:?}"
    );
    for digest in digests {
        assert_eq!(digest.len(), 64, "sha256 {digest:?} is not 64 characters");
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "sha256 {digest:?} is not hex"
        );
    }
}

/// The half of the generator the sentinel path cannot exercise: pulling each
/// target's digest out of the `SHA256SUMS` manifest the release job builds.
///
/// This is where a real break would live. The lookup is a `sed` expression over
/// a manifest whose lines are written by `sha256sum`, and if it stops matching
/// the formula still generates — with sentinel zeros, or with the wrong
/// target's digest — and Homebrew rejects the download for everyone.
#[test]
fn homebrew_formula_takes_its_digests_from_the_manifest() {
    let version = env!("CARGO_PKG_VERSION");
    let targets: Vec<String> = build_matrix().into_iter().map(|t| t.triple).collect();
    let (manifest, digests) = manifest_for("complete", version, &targets);

    let formula = generate_formula(&[
        "--version",
        version,
        "--checksums",
        manifest.to_str().expect("temp path should be UTF-8"),
    ]);
    fs::remove_dir_all(manifest.parent().expect("manifest has a parent")).ok();

    for digest in &digests {
        assert!(
            formula.contains(digest.as_str()),
            "the formula did not pick up {digest} from the manifest:\n{formula}"
        );
    }
    assert!(
        !formula.contains(&"0".repeat(64)),
        "a sentinel digest survived a run with a real manifest:\n{formula}"
    );
}

/// A manifest missing a target must fail the generator outright.
///
/// This is the shape of failure that gets shipped. `sha_for` reports the miss
/// and exits — but called from inside a command substitution within the heredoc
/// that prints the formula, that exit ends the *subshell*. Unless the digests
/// are resolved before the heredoc, `set -e` never sees a failure, the formula
/// is emitted with an empty `sha256 ""`, and the generator exits 0. The release
/// step then reports success and pushes a formula Homebrew refuses to install
/// from, with the only trace an error line partway down a green log.
#[test]
fn homebrew_formula_refuses_a_manifest_with_a_target_missing() {
    let version = env!("CARGO_PKG_VERSION");
    let mut targets: Vec<String> = build_matrix().into_iter().map(|t| t.triple).collect();
    let dropped = targets.pop().expect("the matrix has targets");
    let (manifest, _) = manifest_for("partial", version, &targets);

    let out = run_generator(&[
        "--version",
        version,
        "--checksums",
        manifest.to_str().expect("temp path should be UTF-8"),
    ]);
    fs::remove_dir_all(manifest.parent().expect("manifest has a parent")).ok();

    let formula = String::from_utf8_lossy(&out.stdout);
    assert!(
        !formula.contains("sha256 \"\""),
        "the generator emitted an empty digest for {dropped} instead of failing:\n{formula}"
    );
    assert!(
        !out.status.success(),
        "the generator exited 0 with no digest for {dropped}; the release step would \
         call that a success and push the formula"
    );
}

/// Every npm package name declared in `TARGETS`, the wrapper's source of truth
/// for which platforms ship a binary.
fn npm_targets() -> BTreeSet<String> {
    let js = read("npm/pristine/lib/platform.cjs");
    let packages: BTreeSet<String> = js
        .lines()
        .filter_map(|line| line.split_once("package: '"))
        .filter_map(|(_, rest)| rest.split_once('\''))
        .map(|(name, _)| name.to_owned())
        .collect();
    assert!(
        !packages.is_empty(),
        "found no `package: '…'` entries in platform.cjs — did the TARGETS shape change?"
    );
    packages
}

/// The release matrix, the npm target list, and the directories on disk must
/// name the same set of platforms.
///
/// Each of the three fails differently and none of them fails loudly. A triple
/// the matrix builds with no npm package to stage it into loses that platform
/// from npm while Homebrew keeps working. A package in `TARGETS` with no matrix
/// row publishes an *empty* platform package, because a `files` entry naming a
/// file that does not exist is skipped in silence — the user meets it as
/// `pristine: command not found`. And a `TARGETS` entry with no directory is an
/// `optionalDependency` that resolves to nothing, which npm also skips quietly.
#[test]
fn the_release_matrix_covers_every_npm_platform_package() {
    let matrix = build_matrix();

    let from_matrix: BTreeSet<String> = matrix
        .iter()
        .map(|t| format!("@agentender/{}", t.npm_dir))
        .collect();
    assert_eq!(
        from_matrix,
        npm_targets(),
        "release.yml's `npm:` rows and platform.cjs's TARGETS name different packages"
    );

    for target in &matrix {
        let dir = format!("{ROOT}/npm/{}", target.npm_dir);
        assert!(
            fs::metadata(format!("{dir}/package.json")).is_ok(),
            "{} builds into npm/{}, which has no package.json",
            target.triple,
            target.npm_dir
        );
    }
}

/// Every target is built on a runner of its own platform.
///
/// The build job runs the binary it produced to check its `--version` against
/// the tag, which is only possible natively. Cross-compiling would still produce
/// four tarballs and would silently skip the one check that can tell a shipped
/// binary from a mistagged one.
#[test]
fn every_target_is_built_on_a_runner_of_its_own_platform() {
    for target in build_matrix() {
        let expected_host = if target.triple.contains("apple-darwin") {
            "macos"
        } else {
            "ubuntu"
        };
        assert!(
            target.runner.starts_with(expected_host),
            "{} is built on {}, which cannot run the binary it produces",
            target.triple,
            target.runner
        );
    }
}

/// A platform package must not be published without its binary inside it.
///
/// This is the sharpest edge in the npm half and it is entirely silent: `files`
/// entries naming a missing file are skipped without a warning or a non-zero
/// exit, so a release whose cross-compile failed publishes a manifest and
/// nothing else. The only place to catch it is between `npm pack` and
/// `npm publish`, against the tarball itself.
#[test]
fn the_release_proves_each_platform_tarball_carries_its_binary() {
    let yml = read(".github/workflows/release.yml");
    assert!(
        yml.contains("tar -tzf") && yml.contains("package/pristine"),
        "release.yml must inspect each packed platform tarball for `package/pristine` \
         before publishing it; a `files` entry for a missing binary is skipped in silence"
    );
}

/// Every path handed to `npm pack` is relative-qualified.
///
/// `npm pack npm/pristine` does not pack `npm/pristine`. It is a valid GitHub
/// shorthand — owner `npm`, repo `pristine` — and npm resolves it as one, dying
/// on `ls-remote ssh://git@github.com/npm/pristine.git: Repository not found`.
/// Every npm path in this repository begins `npm/`, so every one of them is
/// ambiguous, and the `./` that disambiguates is exactly the character a later
/// edit drops as noise.
///
/// It fails loudly rather than silently, but it fails in the release job, after
/// the binaries are built and the GitHub Release is already out.
#[test]
fn npm_pack_is_never_handed_an_owner_slash_repo_shorthand() {
    let yml = read(".github/workflows/release.yml");
    for line in yml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some((_, rest)) = trimmed.split_once("npm pack ") else {
            continue;
        };
        let spec = rest.split_whitespace().next().unwrap_or_default();
        assert!(
            !spec.starts_with("npm/"),
            "`npm pack {spec}` is a GitHub owner/repo shorthand, not a path. \
             Write `./{spec}`:\n    {trimmed}"
        );
    }
}

/// The tap push authenticates the host it is pushing to, against keys pinned in
/// this repository.
///
/// `ssh-keyscan github.com >> known_hosts` is the natural thing to write here
/// and it authenticates nothing: it learns the host key from the very
/// connection it is meant to be checking, so whoever answers the scan becomes
/// the trusted host for the push that follows. Someone able to intercept the
/// runner's network then receives the formula — and the deploy key's
/// authentication attempt — while the job reports a clean push to the tap.
///
/// The pinned file is fetched over HTTPS from `api.github.com/meta`, where a
/// certificate chain vouches for it, and committed. That moves the trust
/// decision to review time from a scan re-run on every release.
#[test]
fn the_tap_push_checks_the_host_key_against_pinned_content() {
    let yml = read(".github/workflows/release.yml");
    for line in yml.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed.starts_with('#') || !trimmed.contains("ssh-keyscan"),
            "`ssh-keyscan` learns the host key from the connection it is checking, which is \
             not a check. Pin the keys instead:\n    {trimmed}"
        );
    }
    assert!(
        yml.contains("StrictHostKeyChecking=yes"),
        "the tap push must set StrictHostKeyChecking=yes; `accept-new` is the same \
         trust-on-first-use the pinned file exists to remove"
    );
    assert!(
        yml.contains("UserKnownHostsFile=") && yml.contains(KNOWN_HOSTS),
        "the tap push must point UserKnownHostsFile at {KNOWN_HOSTS}"
    );

    let pinned = read(KNOWN_HOSTS);
    let entries: Vec<&str> = pinned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert!(
        !entries.is_empty(),
        "{KNOWN_HOSTS} has no entries; an empty known_hosts under \
         StrictHostKeyChecking=yes fails every push"
    );
    for entry in &entries {
        let mut fields = entry.split_whitespace();
        let host = fields.next().unwrap_or_default();
        let algorithm = fields.next().unwrap_or_default();
        let key = fields.next().unwrap_or_default();
        assert_eq!(
            host, "github.com",
            "{KNOWN_HOSTS} pins a host other than github.com: {entry}"
        );
        assert!(
            algorithm.starts_with("ssh-") || algorithm.starts_with("ecdsa-"),
            "{KNOWN_HOSTS} entry has no key algorithm: {entry}"
        );
        assert!(
            key.len() > 40,
            "{KNOWN_HOSTS} entry has no key material: {entry}"
        );
    }
}

/// The documented way to repair the tap by hand has to actually commit the
/// formula.
///
/// The tap holds no repair workflow — one would need a cross-repository
/// credential — so this procedure is the fallback when the deploy-key push does
/// not land, and it is reached exactly when something has already gone wrong.
///
/// `git commit -am` is the trap, and it fails in the worst available way. The
/// formula is a NEW file in the tap, `-a` stages only files git already tracks,
/// so the command commits nothing, prints "nothing added to commit", and **exits
/// 0** — which lets a following `&& git push` run and push nothing. The operator
/// is told the repair worked and `brew install` still resolves to the old
/// release. Verified, not assumed.
#[test]
fn the_documented_tap_repair_procedure_stages_the_formula() {
    let docs = read("docs/releasing.md");
    let block = docs
        .split("```")
        .find(|b| b.contains("gh release download"))
        .expect("docs/releasing.md must document how to repair the tap by hand");

    assert!(
        block.contains("git add "),
        "the repair procedure must `git add` the formula. It is a new file in the tap, so \
         `commit -a` stages nothing, exits 0, and a chained push pushes nothing:\n{block}"
    );
    assert!(
        !block.contains("commit -a"),
        "`git commit -a` cannot stage a formula the tap does not track yet, and it exits 0 \
         while doing nothing:\n{block}"
    );
    assert!(
        block.contains("mkdir -p Formula"),
        "create Formula/ explicitly rather than relying on `gh release download --output` to \
         make the parent directory; that is gh behaviour, not a documented guarantee:\n{block}"
    );

    // The repair workflow this replaced was deleted because it authenticated
    // with the tap's own GITHUB_TOKEN, which cannot read a private sibling's
    // release assets. A pointer to it left behind sends someone to a file that
    // is not there, at the moment they are already recovering from a failure.
    for file in [".github/workflows/release.yml", "docs/releasing.md"] {
        assert!(
            !read(file).contains("update-formula"),
            "{file} still points at update-formula.yml, which was deleted from the tap"
        );
    }
}

/// A pre-release tag must not move what `brew install` resolves to.
///
/// A `v0.1.0-rc.1` exercises the whole binary pipeline without spending a
/// crates.io version or an npm version, which is what makes a rehearsal cheap.
/// Pushing its formula into the tap would hand every `brew upgrade` a release
/// candidate, and neither Homebrew nor the tap would say anything was unusual.
#[test]
fn a_prerelease_tag_does_not_touch_the_tap_or_the_registries() {
    let yml = read(".github/workflows/release.yml");
    let guards = yml.matches("!contains(").count();
    assert!(
        guards >= 3,
        "expected the tap push, the crate publish and the npm publish to each be gated \
         off pre-release tags, found {guards} `!contains(` guards in release.yml"
    );
}

/// `cargo install pristine-cli` only works if something pushes the crate, and
/// the token it uses is worth pinning down.
///
/// A stored `CARGO_REGISTRY_TOKEN` is a long-lived credential for the one step
/// in this pipeline that cannot be undone — a crates.io version can be yanked
/// but never replaced. `crates-io-auth-action` exchanges the job's OIDC identity
/// for a short-lived token and revokes it in its post step, so there is nothing
/// left to leak.
#[test]
fn the_crate_publishes_over_oidc_rather_than_a_stored_token() {
    let yml = read(".github/workflows/release.yml");
    assert!(
        yml.contains("cargo publish"),
        "release.yml must actually run `cargo publish`, or nothing ships the crate"
    );
    assert!(
        yml.contains("crates-io-auth-action"),
        "the crate publish must mint its token over OIDC rather than read a stored \
         CARGO_REGISTRY_TOKEN"
    );
    assert!(
        !yml.contains("secrets.CARGO_REGISTRY_TOKEN"),
        "a stored CARGO_REGISTRY_TOKEN defeats the point of the OIDC exchange"
    );
}

/// Parse a manifest as TOML.
///
/// Deliberately a real parser, unlike `build_matrix` above. `nx release version`
/// round-trips `packages/pristine/Cargo.toml` through `@ltd/j-toml`, which
/// reserializes every string with single quotes (brain:
/// `areas/pristine/docs/releasing.md`, "Things that bite"). Cargo and crates.io
/// read that manifest exactly as they read the double-quoted one, so a reader
/// here that goes looking for `"` does not catch a real problem — it invents one,
/// and it invents it on the release commit, which is the worst place to spend a
/// red build.
fn manifest(source: &str, what: &str) -> toml::Table {
    source
        .parse()
        .unwrap_or_else(|e| panic!("{what} should be valid TOML: {e}"))
}

/// Extract the `[package]` string array assigned to `key`.
fn cargo_string_array(cargo: &toml::Table, key: &str) -> Vec<String> {
    cargo
        .get("package")
        .and_then(|package| package.get(key))
        .unwrap_or_else(|| panic!("Cargo.toml [package] has no `{key}`"))
        .as_array()
        .unwrap_or_else(|| panic!("Cargo.toml [package] `{key}` is not an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("Cargo.toml [package] `{key}` holds a non-string"))
                .to_owned()
        })
        .collect()
}

/// crates.io rejects a publish whose manifest breaks its metadata rules, and
/// the rejection only ever shows up in the one job that is hard to re-run —
/// the irreversible one, at the end of a release that has already shipped its
/// binaries. Assert the constraints here instead.
#[test]
fn crate_metadata_is_crates_io_publishable() {
    let cargo = manifest(
        &fs::read_to_string("Cargo.toml").expect("the crate manifest should be readable"),
        "the crate manifest",
    );
    let workspace = manifest(&read("Cargo.toml"), "the workspace manifest");

    let keywords = cargo_string_array(&cargo, "keywords");
    assert!(
        (1..=5).contains(&keywords.len()),
        "crates.io allows at most 5 keywords, found {}: {keywords:?}",
        keywords.len()
    );
    for kw in &keywords {
        assert!(
            !kw.is_empty() && kw.len() <= 20,
            "crates.io caps a keyword at 20 characters: {kw:?}"
        );
        assert!(
            kw.starts_with(|c: char| c.is_ascii_alphanumeric()),
            "a keyword must start alphanumeric: {kw:?}"
        );
    }

    assert!(
        !cargo_string_array(&cargo, "categories").is_empty(),
        "declare at least one crates.io category"
    );

    // A field may be stated outright or inherited with `field.workspace = true`,
    // in which case the root manifest's `[workspace.package]` has to actually
    // carry it. An inherited key the workspace never declares is a
    // `cargo publish` failure and nothing earlier — `cargo build` does not read
    // publish metadata.
    for field in ["readme", "repository", "license", "description"] {
        let declared = cargo
            .get("package")
            .and_then(|package| package.get(field))
            .unwrap_or_else(|| {
                panic!(
                    "Cargo.toml [package] is missing `{field}`, which `cargo install` users read"
                )
            });
        if declared.is_str() {
            continue;
        }
        assert!(
            declared
                .get("workspace")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            "Cargo.toml [package] `{field}` is neither a string nor `{field}.workspace = true`"
        );
        assert!(
            workspace
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|package| package.get(field))
                .is_some_and(toml::Value::is_str),
            "the crate inherits `{field}` from the workspace, which does not declare it"
        );
    }
}

/// Every third-party action is named by an immutable commit, not by a tag.
///
/// A tag is a *mutable pointer its owner controls*, so `@v4` means "whatever
/// that account publishes next", evaluated fresh on every run. That is an
/// ordinary convenience in most repositories and something else entirely in
/// this one: `release.yml` runs with `contents: write` and `id-token: write`,
/// hands one step an SSH deploy key for the tap, and ends in the two steps that
/// cannot be undone — a crates.io version can be yanked but never replaced, and
/// an npm version can be unpublished only within 72 hours. An action that moved
/// under us could rewrite the repository, mint a Sigstore signature over
/// something nobody built, swap the release assets after they were checksummed,
/// or publish an unrecoverable version. None of it would look unusual in a log,
/// because every one of those is a thing the job is legitimately allowed to do.
///
/// A SHA is the only reference GitHub will not re-point. The `# vX.Y.Z` comment
/// is required alongside it because a bare 40-hex string tells a reviewer
/// nothing about what it is or whether it is current, and it is what Dependabot
/// reads to offer the bump.
///
/// This covers **every** workflow rather than only the one holding credentials.
/// Exempting CI would make the rule a judgement call about which permissions
/// matter, re-made by hand each time a workflow is added — and the workflow that
/// gets it wrong is the one written in a hurry.
#[test]
fn every_third_party_action_is_pinned_to_a_commit() {
    let dir = format!("{ROOT}/.github/workflows");
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("{dir}: {e}"));

    let mut checked = 0;
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|e| e != "yml") {
            continue;
        }
        let name = path.display().to_string();
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));

        for line in body.lines() {
            // A `uses:` KEY, not the word. It opens a line, optionally after the
            // `- ` of a step, and anything else on the line is a value or a
            // trailing comment. Matching `uses:` anywhere would also catch the
            // prose in ci.yml explaining why these are pinned — a scanner that
            // fails on a comment about itself teaches people to loosen it.
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some(reference) = line
                .strip_prefix("- uses:")
                .or_else(|| line.strip_prefix("uses:"))
            else {
                continue;
            };
            let reference = reference.trim();
            // A reusable workflow in this same repository.
            if reference.starts_with("./") {
                continue;
            }
            checked += 1;

            let (action, rest) = reference
                .split_once('@')
                .unwrap_or_else(|| panic!("{name}: `uses: {reference}` has no version at all"));
            let (revision, comment) = match rest.split_once('#') {
                Some((rev, c)) => (rev.trim(), c.trim()),
                None => (rest.trim(), ""),
            };

            let pinned = revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit());
            assert!(
                pinned,
                "{name}: `{action}@{revision}` is a mutable tag. Pin it to the commit \
                 that tag points at today:\n    \
                 gh api repos/{action}/commits/{revision} --jq .sha"
            );
            assert!(
                comment.starts_with('v'),
                "{name}: `{action}` is pinned to {revision} with no `# vX.Y.Z` comment, \
                 so nothing says which release it is or whether it is current"
            );
        }
    }

    assert!(
        checked >= 10,
        "only found {checked} third-party action references — did the scan stop working?"
    );
}
