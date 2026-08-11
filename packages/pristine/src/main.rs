//! `pristine` finds reclaimable build artifacts and vendored dependency directories across
//! every ecosystem on a machine, and tells you what regenerates each one before you delete it.
//!
//! The command line here is deliberately thin — the rollup tree TUI (#602) is the real front
//! end — but several of the promises made elsewhere are properties of the *program* rather
//! than of the library, and no library test can hold them.
//!
//! Properties of the output:
//!
//! - A tier-two hit says out loud that it does not know how to regenerate what it found. The
//!   asymmetry against tier one is information rather than an omission.
//! - A tier that could not run says `inert` rather than printing nothing, because silence is
//!   indistinguishable from a clean result and the two mean opposite things.
//! - `--min-size` is a flag a person can type rather than a builder method.
//! - So is `--breakdown`. "How much do I get back" is the headline question, and leaving the
//!   sizing modes reachable only from the library made the answer unavailable at any price.
//!   `--breakdown-under <PATH>` is the same answer for one subtree, at that subtree's cost.
//! - A listing with a dash on it says what the dash means and which flag removes it. A count
//!   of unpriced claims with no way to act on it is a puzzle rather than a report.
//!
//! This front end paints once, at the end, because it sorts by size — so it is the one
//! consumer that cannot show off the streaming underneath it. Claims and their prices arrive
//! as separate events from [`Walker::run`] and are folded back together here. The TUI (#602)
//! is what the split is for: rows the moment they are found, numbers filling in behind.
//!
//! Properties of the run:
//!
//! - `--dry-run` prints the resolved plan and is inert. It prints the same [`Plan`] a real run
//!   executes, so a preview cannot disagree with the run it previews.
//! - The confirmation defaults to **no**, and so does end of input. A script that means to
//!   delete says so with `--yes` rather than by being silent.
//! - A run that could not do everything it was asked exits non-zero. Without that,
//!   `pristine /does/not/exist` prints "0 directories reclaimable" and no script can tell that
//!   from a clean machine except by the status.
//!
//! Selecting *which* directories to remove is the TUI's job. Until it exists `--delete` means
//! all of them — from both tiers — which is why every guard above matters more here than it
//! eventually will.
//!
//! All of that is the *sweep*, which is what a bare `pristine [PATH]` does. `pristine repo`
//! is the second mode: one git checkout, cleaned the way `git clean -fdx` cleans it, with the
//! reset verbs the sweep has no concept of. Its own promises, none of which a library test can
//! hold either:
//!
//! - Vendor and env are excluded from a list the user *did* ask for, unless they say
//!   otherwise. `node_modules` costs minutes and a network to get back, and nothing at all
//!   regenerates a `.env`.
//! - Any action flag makes the run non-interactive, so nothing in CI can hang on a prompt.
//! - `--yes` gates the final confirmation and nothing else, so `pristine repo --ignored` in CI
//!   still refuses to delete without it.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use pristine::delete::confirm;
use pristine::repo::{Class, Repo, Reset, Selected, Selection};
use pristine::{
    DEFAULT_MIN_SIZE, Deleter, Enumeration, FallbackReport, Found, Hit, Plan, Planner, Removal,
    Ruleset, Size, SizeMode, Target, WalkOutcome, Walker,
};

/// A language-agnostic reclaimable-space finder and cleaner.
#[derive(Debug, Parser)]
#[command(name = "pristine", version)]
// The sweep's flags and `repo`'s are two vocabularies for two jobs, and a run is one job.
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// The mode. Omitted, the sweep below runs.
    #[command(subcommand)]
    mode: Option<Mode>,

    /// The sweep: what a bare `pristine [PATH]` does.
    #[command(flatten)]
    sweep: Sweep,
}

/// The mode that is not the sweep.
#[derive(Debug, Subcommand)]
enum Mode {
    /// Clean one git checkout: the `git clean -fdx` replacement.
    ///
    /// Nothing here is enumerated by pristine. `git clean -n -d` and `git clean -n -d -X` are
    /// the authority, so nested ignore files, negations, `info/exclude`, the global excludes
    /// and the refusal to touch a nested repository are inherited exactly rather than
    /// reimplemented.
    ///
    /// With no action flag it asks; with any of them it does not, so nothing in CI hangs on a
    /// prompt.
    Repo(RepoArgs),
}

/// The sweep's arguments.
#[derive(Debug, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, and the lint's advice — fold them into a state \
              machine — would take the flags off the command line"
)]
struct Sweep {
    /// The directory to scan.
    #[arg(default_value = ".", value_name = "PATH")]
    root: PathBuf,

    /// How big a gitignored directory has to be before the fallback tier claims it.
    ///
    /// Plain bytes, or a suffix: K/M/G/T and KiB/MiB/GiB/TiB are 1024-based, KB/MB/GB/TB are
    /// 1000-based. The floor applies to the fallback tier only — a rule that names a directory
    /// has already said it is output, and an empty node_modules is still a node_modules.
    #[arg(
        long,
        value_name = "SIZE",
        default_value_t = DEFAULT_MIN_SIZE,
        value_parser = parse_size,
    )]
    min_size: u64,

    /// Put a number on every claim, by walking each one.
    ///
    /// A scan does not do this unasked, and the reason is not caution: there is no recursive
    /// directory size on any platform this runs on, so a price is a full enumeration of the
    /// subtree the scan just pruned at. Measured over one real `~/repos`: 4.6 s and 14.0 GiB
    /// priced without it, 55.8 s and 165.1 GiB with.
    #[arg(long, conflicts_with = "breakdown_under")]
    breakdown: bool,

    /// Put a number on the claims under PATH only, and leave the rest of the scan unpriced.
    ///
    /// The whole scan still runs and every claim is still listed — this buys a price for one
    /// subtree at that subtree's cost, rather than making the answer to "how much is in here"
    /// cost a walk of everything. PATH must be inside the scan root.
    #[arg(long, value_name = "PATH")]
    breakdown_under: Option<PathBuf>,

    /// Remove everything the scan found, after showing the plan and asking.
    #[arg(long)]
    delete: bool,

    /// Print the plan and stop. Nothing is removed, whatever else is passed.
    #[arg(long)]
    dry_run: bool,

    /// Answer the confirmation with yes. The only way a script gets to delete anything.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Keep anything touched more recently than this.
    ///
    /// A whole number and a unit: `h` hours, `d` days, `w` weeks, `m` months (30 days), `y`
    /// years (365 days). There are no minutes, which is what lets `m` mean months without
    /// the `m`/`M` ambiguity that would otherwise need a case-sensitive flag.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    older_than: Option<Duration>,

    /// Whether to stay on the filesystem the scan root is on. Pass `--one-file-system=false`
    /// to follow a mount, which is how a sweep of one project reaches a network share or a
    /// backup volume that happens to be mounted inside it.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    one_file_system: bool,
}

impl Sweep {
    /// How hard the scan should work for each claim's size.
    ///
    /// Unpriced is the default and stays the default. What the two flags buy is that the
    /// product's headline question — how much do I get back — is answerable at all, at a price
    /// the user picks: everything, or one subtree.
    fn size_mode(&self) -> Result<SizeMode, String> {
        match &self.breakdown_under {
            Some(scope) => Ok(SizeMode::BreakdownUnder(anchor(&self.root, scope)?)),
            None if self.breakdown => Ok(SizeMode::Breakdown),
            None => Ok(SizeMode::Skip),
        }
    }
}

/// Spells `scope` the way the walk will spell the hits it is compared against.
///
/// The comparison is by path, and a hit carries the scan root exactly as it was typed: point
/// pristine at `.` and every hit begins `./`. So neither side can simply be resolved. Both are
/// resolved *here*, only to work out where the scope sits inside the root, and the answer is
/// then re-anchored onto the root as given.
///
/// Getting this wrong is silent and it fails in the direction that reads as good news: a scope
/// that matches no hit prices nothing, and a listing of dashes looks exactly like a subtree
/// with nothing in it. A scope that resolves outside the scan is refused for the same reason.
fn anchor(root: &Path, scope: &Path) -> Result<PathBuf, String> {
    let resolved_root = std::fs::canonicalize(root)
        .map_err(|err| format!("the scan root {}: {err}", root.display()))?;
    let resolved_scope = std::fs::canonicalize(scope)
        .map_err(|err| format!("--breakdown-under {}: {err}", scope.display()))?;
    let relative = resolved_scope.strip_prefix(&resolved_root).map_err(|_| {
        format!(
            "--breakdown-under {} is not inside the scan root {}; it prices a subtree of the \
             scan, not a second tree",
            scope.display(),
            root.display()
        )
    })?;
    Ok(root.join(relative))
}

/// Repo mode's arguments.
#[derive(Debug, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, and the lint's advice — fold them into a state \
              machine — would take the flags off the command line"
)]
struct RepoArgs {
    /// A path inside the checkout to clean.
    ///
    /// The whole work tree is cleaned wherever inside it this points, because `git clean`
    /// scoped to a subdirectory cleans only that subtree while `git reset --hard` resets
    /// everything regardless — one run cannot mean two different things by "here".
    #[arg(default_value = ".", value_name = "PATH")]
    path: PathBuf,

    /// Discard changes to tracked files: `worktree` keeps the index, `hard` discards it too.
    /// Bare `--reset` means `hard`.
    #[arg(
        long,
        value_name = "SCOPE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "hard",
    )]
    reset: Option<ResetScope>,

    /// Remove untracked files: what `git clean -n -d` lists.
    #[arg(long)]
    untracked: bool,

    /// Remove ignored files: what `git clean -n -d -X` lists.
    #[arg(long)]
    ignored: bool,

    /// Include vendored dependency directories (`node_modules`). Off unless asked for.
    #[arg(
        long = "node-modules",
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    node_modules: Option<bool>,

    /// Include env files (`*.env*`). Off unless asked for: nothing regenerates one.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    env: Option<bool>,

    /// Print the plan and stop. Nothing is removed and nothing is reset.
    #[arg(long)]
    dry_run: bool,

    /// Answer the confirmation with yes. It gates that and nothing else — it selects nothing,
    /// so `pristine repo --yes` on its own still removes nothing.
    #[arg(long, short = 'y')]
    yes: bool,
}

/// How far a reset goes, as the flag spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ResetScope {
    /// Discard working-tree changes only.
    Worktree,
    /// Discard everything, index included.
    Hard,
}

impl From<ResetScope> for Reset {
    fn from(scope: ResetScope) -> Self {
        match scope {
            ResetScope::Worktree => Self::WorkTree,
            ResetScope::Hard => Self::Hard,
        }
    }
}

impl RepoArgs {
    /// Whether the command line, rather than a person, is resolving the plan.
    ///
    /// The presence of *any* of these makes the run non-interactive, which is the whole reason
    /// the rule exists: a prompt in CI is a hang, and a hang is worse than a refusal. The two
    /// modifiers count, because passing one is a statement about the plan even though it
    /// selects nothing on its own.
    ///
    /// **`--yes` counts too, and leaving it out was a hole.** It selects nothing — see
    /// [`RepoArgs::selection`], which never reads it — so `pristine repo --yes` still resolves
    /// to an empty selection and does nothing. But if it did not make the run non-interactive,
    /// `--yes` would put the cascade and the skipped confirmation in the same run: a person
    /// could be asked what to clean and then never asked to confirm it, because the flag that
    /// was supposed to be the answer to the final question had already answered it in advance.
    /// That turns "I consent to what I asked for" into "I consent to whatever I am about to be
    /// asked", which is the one reading of consent this must not have.
    fn chosen(&self) -> bool {
        self.reset.is_some()
            || self.untracked
            || self.ignored
            || self.node_modules.is_some()
            || self.env.is_some()
            || self.yes
    }

    fn selection(&self) -> Selection {
        Selection {
            reset: self.reset.map(Reset::from),
            untracked: self.untracked,
            ignored: self.ignored,
            vendor: self.node_modules.unwrap_or(false),
            env: self.env.unwrap_or(false),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let done = match &cli.mode {
        Some(Mode::Repo(args)) => clean(args, &mut out),
        None => run(&cli.sweep, &mut out),
    };
    match done {
        // Anything the run could not do — a path it could not read, a directory it could not
        // remove — is a lower bound reported as a total unless the status says otherwise. A
        // listing that is a lower bound must not look, to a script, like the whole truth.
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("pristine: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether everything asked for actually happened — both halves of it. A scan that
/// could not read everything and a removal that could not finish are different failures with
/// the same consequence, and the exit status is the only place a script reads either.
fn run(cli: &Sweep, out: &mut impl Write) -> Result<bool, Box<dyn std::error::Error>> {
    let ruleset = Arc::new(Ruleset::load(None)?);
    let hits = Mutex::new(Vec::new());
    let sizes = Mutex::new(HashMap::new());
    // The mount rule has to reach the walk as well as the plan. Setting it on only one of them
    // makes `--one-file-system=false` a flag that permits crossing a mount the scan never
    // looked across, which reads as "there was nothing over there".
    let outcome = Walker::new(&cli.root, ruleset)
        .same_file_system(cli.one_file_system)
        .min_size(cli.min_size)
        .size_mode(cli.size_mode()?)
        // A claim arrives as soon as it is judged and its price arrives later, from the
        // pricing pool. A TUI would repaint the row; this front end paints once, at the end,
        // so it holds the prices and folds them in below. `run` does not return until the
        // pool has drained, so nothing here is racing it.
        .run(|found| match found {
            Found::Claim(hit) => lock(&hits).push(hit),
            Found::Priced(priced) => {
                lock(&sizes).insert(priced.path, priced.size);
            }
        });

    let mut hits = hits.into_inner().unwrap_or_else(PoisonError::into_inner);
    let sizes = sizes.into_inner().unwrap_or_else(PoisonError::into_inner);
    for hit in &mut hits {
        if let Some(size) = sizes.get(&hit.path) {
            hit.size = *size;
        }
    }
    // Biggest first, and the unpriced after the priced rather than ahead of them: a tier-one
    // claim has no number because nothing looked, not because it is empty.
    hits.sort_by(|a, b| {
        b.size
            .bytes()
            .cmp(&a.size.bytes())
            .then_with(|| a.path.cmp(&b.path))
    });

    let whole = outcome.errors.is_empty();

    if !cli.delete && !cli.dry_run {
        for hit in &hits {
            writeln!(out, "{}", row(hit, &cli.root))?;
        }
        writeln!(out, "{}", summary(&hits))?;
        report_unpriced(out, unpriced(&hits))?;
        report_fallback(out, &outcome.fallback)?;
        report_scan(out, &outcome)?;
        return Ok(whole);
    }

    // Both tiers' hits go into one plan. Tier two is not a second pass and not a second
    // planner: by the time the walk has returned, which tier claimed a directory is a fact
    // about how it was found and no longer a fact about how it is removed.
    let plan = Planner::new(&cli.root)
        .one_file_system(cli.one_file_system)
        .older_than(cli.older_than)
        .plan(hits.iter().map(Target::from));
    write_plan(out, &plan, DIRECTORY)?;
    // Said here rather than inside `write_plan`, which repo mode shares: repo mode prices
    // nothing and has no breakdown flag, so pointing its reader at one would be a dead end.
    report_unpriced(out, plan.unpriced())?;
    // Beside the plan for the same reason it sits beside the listing, and with more at stake:
    // a tier that went inert is a tier whose findings are missing from what is about to be
    // removed, and a plan that did not say so would read as the whole of what is reclaimable.
    report_fallback(out, &outcome.fallback)?;
    report_scan(out, &outcome)?;

    if cli.dry_run {
        writeln!(out, "\ndry run: nothing was removed")?;
        return Ok(whole);
    }
    if plan.is_empty() {
        writeln!(out, "\nnothing was removed")?;
        return Ok(whole);
    }
    if !cli.yes {
        let question = format!("\nRemove {}?", plural(plan.targets().len(), DIRECTORY));
        let stdin = std::io::stdin();
        if !confirm(&question, &mut stdin.lock(), out)? {
            writeln!(out, "nothing was removed")?;
            return Ok(whole);
        }
    }

    let removal = Deleter::new().remove(&plan);
    write_removal(out, &removal, plan.root(), DIRECTORY)?;
    for failure in &removal.failures {
        eprintln!("pristine: {}: {}", failure.path.display(), failure.message);
    }
    Ok(whole && removal.is_clean())
}

// ------------------------------------------------------------------------------------------
// Repo mode.
// ------------------------------------------------------------------------------------------

/// Cleans one checkout.
///
/// The order is the one the Node predecessor established, and **the reset happens first**. What
/// that costs is the thing this function is shaped around: `git clean` answers out of the
/// index, the reset MOVES the index, so a plan built before a reset is a plan about a
/// repository that no longer exists.
///
/// It is not a theoretical window. `git rm --cached tracked.txt` leaves a committed file on
/// disk and out of the index, so `git clean -n -d` reports it as untracked and it lands on the
/// plan — and then `git reset --hard HEAD` puts it back, making it tracked. Executing the
/// original plan deletes a committed file, which is the one thing `git clean` semantics exist
/// to make impossible.
///
/// So the enumeration does not outlive the reset. After the reset the work tree is asked again,
/// and the answer is narrowed to what the user was shown and confirmed — see [`reconsider`].
/// Both halves are load-bearing: re-asking is what stops a now-tracked file being deleted, and
/// narrowing is what stops a *newly* collapsed directory being deleted without ever appearing
/// on a plan anybody saw.
///
/// Untracked and ignored, by contrast, go into ONE plan rather than two sequential ones. The
/// design says "untracked, then ignored" and the two lists are disjoint by construction, so the
/// order between them is unobservable — while a single plan buys the nested-target dedup that
/// keeps a target contained by another from being reported as a failure for a directory that
/// is gone because the plan worked.
fn clean(args: &RepoArgs, out: &mut impl Write) -> Result<bool, Box<dyn std::error::Error>> {
    let repo = Repo::discover(&args.path)?;
    let enumeration = repo.enumerate()?;

    let selection = if args.chosen() {
        args.selection()
    } else {
        let stdin = std::io::stdin();
        ask(&mut stdin.lock(), out)?
    };
    if selection.is_empty() {
        writeln!(out, "nothing selected, so nothing was removed")?;
        return Ok(true);
    }

    let selected = pristine::repo::select(&enumeration, &selection);
    // The work tree root, not the path the user typed: the plan's under-root check is what
    // keeps every target inside the checkout, and the checkout is what repo mode cleans.
    let mut plan = Planner::new(repo.root()).plan(selected.targets.iter().cloned());
    write_repo_plan(out, &plan, selection, &selected, &enumeration)?;

    if args.dry_run {
        if selection.reset.is_some() {
            // A dry run does not reset, so it cannot show what the work tree looks like
            // afterwards. Saying so keeps the preview honest: the list above is what a real
            // run starts from, and the reset can only take rows off it.
            writeln!(
                out,
                "note: a real run re-asks git after the reset, so the list above is an upper \
                 bound"
            )?;
        }
        writeln!(out, "\ndry run: nothing was reset and nothing was removed")?;
        return Ok(true);
    }
    if selection.reset.is_none() && plan.is_empty() {
        writeln!(out, "\nnothing was removed")?;
        return Ok(true);
    }
    if !args.yes
        && !confirm(
            &question(selection, &plan),
            &mut std::io::stdin().lock(),
            out,
        )?
    {
        writeln!(out, "nothing was reset and nothing was removed")?;
        return Ok(true);
    }

    if let Some(reset) = selection.reset {
        repo.reset(reset)?;
        // The plan above already said which command this is. What is worth saying here is only
        // that it ran, because it ran before the removal and the removal may yet report
        // something.
        writeln!(out, "reset: done")?;
        // ...and the index has moved under the plan. Ask again.
        let (refreshed, withdrawn) = reconsider(&repo, selection, &plan)?;
        write_withdrawn(out, &withdrawn, plan.root())?;
        plan = refreshed;
    }
    if plan.is_empty() {
        return Ok(true);
    }
    let removal = Deleter::new().remove(&plan);
    write_removal(out, &removal, plan.root(), PATH)?;
    for failure in &removal.failures {
        eprintln!("pristine: {}: {}", failure.path.display(), failure.message);
    }
    Ok(removal.is_clean())
}

/// Asks the work tree again after a reset, and narrows the answer to what was confirmed.
///
/// Two separate jobs, and each catches a different way the reset can invalidate a plan.
///
/// **Re-asking** catches a target that stopped being removable. `git clean` answers out of the
/// index, so a file that was outside it — `git rm --cached` on a committed file is the ordinary
/// route — is untracked before the reset and tracked after it. It is on the plan and it must
/// not be deleted, and the only authority on that is git, asked again.
///
/// **Narrowing** catches the opposite: a target that appeared, or grew, because of the reset. A
/// hard reset deletes a file that was staged but never committed, and a directory whose last
/// non-removable child was that file is one git now collapses — so `git clean` starts offering
/// the whole directory where it previously offered one child. Deleting it would be deleting
/// something no plan ever showed anyone. Anything not contained by a confirmed target is
/// therefore withdrawn rather than executed, and the user runs again against a listing that is
/// true.
///
/// The comparison is against [`PlanTarget::requested`] — the path as this handed it in, before
/// the planner resolved it — because both sides are built by the same code from the same work
/// tree root, so they are directly comparable without either being canonicalised.
fn reconsider(
    repo: &Repo,
    selection: Selection,
    confirmed: &Plan,
) -> Result<(Plan, Vec<PathBuf>), pristine::RepoError> {
    let enumeration = repo.enumerate()?;
    let approved: Vec<&Path> = confirmed
        .targets()
        .iter()
        .map(|target| target.requested.as_path())
        .collect();

    let mut targets = Vec::new();
    let mut withdrawn = Vec::new();
    for target in pristine::repo::select(&enumeration, &selection).targets {
        // `starts_with` compares whole components, so `out-takes` is not under `out`.
        if approved.iter().any(|ok| target.path.starts_with(ok)) {
            targets.push(target);
        } else {
            withdrawn.push(target.path);
        }
    }
    withdrawn.sort_unstable();
    Ok((Planner::new(repo.root()).plan(targets), withdrawn))
}

/// What the reset put beyond what was confirmed, and what to do about it.
fn write_withdrawn(
    out: &mut impl Write,
    withdrawn: &[PathBuf],
    root: &Path,
) -> std::io::Result<()> {
    if withdrawn.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "withdrawn after the reset: {}, because the reset made them reach past the plan you \
         confirmed. Run again to see them.",
        plural(withdrawn.len(), PATH)
    )?;
    for path in withdrawn {
        let path = path.strip_prefix(root).unwrap_or(path);
        writeln!(out, "  {}", path.display())?;
    }
    Ok(())
}

/// The cascade, asked only when the command line did not answer it.
///
/// Every question defaults to the answer that changes nothing, so a run with no input — a
/// pipe with nothing in it, an `Enter` held down — does nothing at all.
fn ask(input: &mut impl BufRead, output: &mut impl Write) -> std::io::Result<Selection> {
    let reset = ask_reset(input, output)?;
    let untracked = confirm("\nRemove untracked files?", input, output)?;
    let ignored = confirm("\nRemove ignored files?", input, output)?;
    // Asked whenever either list was taken, rather than only under `ignored`: the vendor and
    // env filters apply to both, so an untracked-but-not-ignored `node_modules` is held back
    // by the same rule and has to be askable about by the same question.
    let (vendor, env) = if untracked || ignored {
        (
            confirm(
                "\n  Include vendored dependencies (node_modules)?",
                input,
                output,
            )?,
            confirm("\n  Include env files (*.env*)?", input, output)?,
        )
    } else {
        (false, false)
    };
    writeln!(output)?;
    Ok(Selection {
        reset,
        untracked,
        ignored,
        vendor,
        env,
    })
}

/// The one question with three answers.
///
/// Only `2` and `3` reset anything, on the same principle as [`confirm`]'s "only yes means
/// yes": an answer nobody can read is not an instruction to discard someone's work.
fn ask_reset(input: &mut impl BufRead, output: &mut impl Write) -> std::io::Result<Option<Reset>> {
    writeln!(output, "Reset changed (tracked) files?")?;
    writeln!(output, "  1) No, leave my changes")?;
    writeln!(
        output,
        "  2) Discard working-tree changes only  ({})",
        Reset::WorkTree.command()
    )?;
    writeln!(
        output,
        "  3) Discard everything (hard reset)    ({})",
        Reset::Hard.command()
    )?;
    write!(output, "> [1] ")?;
    output.flush()?;

    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(None);
    }
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "2" | "worktree" => Some(Reset::WorkTree),
        "3" | "hard" => Some(Reset::Hard),
        _ => None,
    })
}

/// The plan, plus the three things about repo mode that a list of targets cannot carry.
fn write_repo_plan(
    out: &mut impl Write,
    plan: &Plan,
    selection: Selection,
    selected: &Selected,
    enumeration: &Enumeration,
) -> std::io::Result<()> {
    if let Some(reset) = selection.reset {
        writeln!(out, "reset: {reset} ({})", reset.command())?;
    }
    write_plan(out, plan, PATH)?;

    // What was held back, and the flag that would have kept it. A count with no way to act on
    // it is a puzzle rather than a report.
    for (count, what, flag) in [
        (selected.vendor, VENDORED, "--node-modules"),
        (selected.env, ENV_FILE, "--env"),
    ] {
        if count > 0 {
            writeln!(
                out,
                "excluded: {} ({flag} includes them)",
                plural(count, what)
            )?;
        }
    }
    // Named rather than counted. git offers a directory whole whenever everything inside it is
    // removable, so the reason one of these is held back is a level down from the row — and a
    // bare count would send the reader looking for something the output never showed them.
    if !selected.concealed.is_empty() {
        writeln!(
            out,
            "held back: {}, because git offered them whole and they hold something you did not \
             ask to remove",
            plural(selected.concealed.len(), PATH)
        )?;
        for concealed in &selected.concealed {
            // The flag that would release it, where one would. An unreadable subtree is not a
            // thing any flag opts into, and saying otherwise would send the reader in a circle.
            let hint = match concealed.reason.class() {
                Some(Class::Vendor) => " (--node-modules includes it)",
                Some(Class::Env) => " (--env includes it)",
                _ => "",
            };
            writeln!(
                out,
                "  {}  —  {}{hint}",
                concealed.path.display(),
                concealed.reason
            )?;
        }
    }
    // Not a refusal of ours — git will not clean a nested repository and neither will this —
    // but a user who does not see it named reads this run as having covered everything.
    if !enumeration.skipped.is_empty() {
        writeln!(
            out,
            "skipped: {} git will not clean",
            plural(enumeration.skipped.len(), NESTED_REPOSITORY)
        )?;
        for path in &enumeration.skipped {
            let path = path.strip_prefix(plan.root()).unwrap_or(path);
            writeln!(out, "  {}", path.display())?;
        }
    }
    Ok(())
}

/// The final confirmation, naming both halves of what is about to happen.
///
/// A reset is irreversible on its own, so a run that only resets still has to ask — and the
/// question has to say so, or a user reading "Remove 0 paths?" would answer it about the wrong
/// thing.
fn question(selection: Selection, plan: &Plan) -> String {
    let removing =
        (!plan.is_empty()).then(|| format!("remove {}", plural(plan.targets().len(), PATH)));
    let resetting = selection.reset.map(|reset| reset.to_string());
    let mut halves: Vec<String> = resetting.into_iter().chain(removing).collect();
    if halves.is_empty() {
        halves.push("do nothing".to_owned());
    }
    let mut question = halves.join(" and ");
    question[..1].make_ascii_uppercase();
    format!("\n{question}?")
}

/// One reclaimable directory, with what is known about getting it back.
fn row(hit: &Hit, root: &Path) -> String {
    let path = hit.path.strip_prefix(root).unwrap_or(&hit.path);
    let regenerate = hit.regenerate().unwrap_or(
        // The asymmetry is the point: this tier knows the directory is safe to remove and
        // knows nothing about what put it there, which tells you the deletion is not cheap.
        "no known way to regenerate this",
    );
    format!(
        "{:>10}  {:<60}  {regenerate}",
        size(hit.size),
        path.display()
    )
}

/// What the scan found, across both tiers.
fn summary(hits: &[Hit]) -> String {
    let priced: u64 = hits.iter().filter_map(|hit| hit.size.bytes()).sum();
    format!(
        "\n{} reclaimable, {} priced, {} not priced",
        plural(hits.len(), DIRECTORY),
        human(priced),
        unpriced(hits),
    )
}

/// How many claims nothing has looked inside.
fn unpriced(hits: &[Hit]) -> usize {
    hits.iter().filter(|hit| hit.size.bytes().is_none()).count()
}

/// What the dash means, and the two ways to turn it into a number.
///
/// "20 not priced" with no way to act on it is a puzzle rather than a report — the same rule
/// repo mode's `excluded:` lines follow, where a count is always printed with the flag that
/// releases it. Nothing is said when everything was priced, because then there is no dash on
/// screen to explain.
fn report_unpriced(out: &mut impl Write, unpriced: usize) -> std::io::Result<()> {
    if unpriced == 0 {
        return Ok(());
    }
    writeln!(
        out,
        "not priced: nothing looked inside. --breakdown prices every claim, --breakdown-under \
         <PATH> just one subtree; both walk what they price."
    )
}

/// What tier two managed, whenever it was asked at all.
///
/// Silence here would be indistinguishable from a clean scan, and the two mean opposite
/// things: a tier that found nothing has looked, and an inert one has not.
fn report_fallback(out: &mut impl Write, fallback: &FallbackReport) -> std::io::Result<()> {
    if !fallback.enabled {
        return Ok(());
    }
    if fallback.is_inert() {
        return writeln!(
            out,
            "fallback tier: inert — nothing scanned is in a git work tree, so nothing could be \
             judged reclaimable by inference (floor was {})",
            human(fallback.min_size)
        );
    }
    let held_back = if fallback.holding_a_checkout == 0 {
        String::new()
    } else {
        format!(
            "; {} left alone because they hold a checkout",
            plural(fallback.holding_a_checkout, DIRECTORY)
        )
    };
    writeln!(
        out,
        "fallback tier: {} found in {} above a {} floor{held_back}",
        plural(fallback.hits, DIRECTORY),
        plural(fallback.work_trees, WORK_TREE),
        human(fallback.min_size),
    )
}

/// The plan, in full. This is what `--dry-run` exists to show, so it lists what would be
/// removed AND what would not: a plan that printed only the first half would be
/// indistinguishable from a clean machine when an age floor or a checkout kept everything.
fn write_plan(out: &mut impl Write, plan: &Plan, unit: Noun) -> std::io::Result<()> {
    // The plan's own root, not the one the user typed: a plan holds RESOLVED paths, so
    // stripping `.` off `/Users/me/repo/node_modules` takes nothing off at all.
    let root = plan.root();
    for target in plan.targets() {
        let path = target.path.strip_prefix(root).unwrap_or(&target.path);
        writeln!(out, "{:>10}  {}", size(target.size), path.display())?;
    }
    writeln!(
        out,
        "\nplan: {}, {} priced, {} not priced",
        plural(plan.targets().len(), unit),
        human(plan.measured_bytes()),
        plan.unpriced(),
    )?;

    if plan.kept().is_empty() {
        return Ok(());
    }
    writeln!(out, "kept: {}", plural(plan.kept().len(), unit))?;
    for refused in plan.kept() {
        let path = refused.path.strip_prefix(root).unwrap_or(&refused.path);
        writeln!(out, "  {}  —  {}", path.display(), refused.reason)?;
    }
    Ok(())
}

fn write_removal(
    out: &mut impl Write,
    removal: &Removal,
    root: &Path,
    unit: Noun,
) -> std::io::Result<()> {
    let complete = removal
        .removed
        .iter()
        .filter(|removed| removed.complete)
        .count();
    writeln!(
        out,
        "\nremoved {}, {} freed",
        plural(complete, unit),
        human(removal.bytes_freed()),
    )?;

    // A subtree the deleter declined to enter is the safety model working rather than a
    // fault, so it is reported here and not as a failure — but it IS reported, because a
    // directory the user selected and did not get is something they need to know about.
    if !removal.kept.is_empty() {
        writeln!(out, "kept {}:", plural(removal.kept.len(), unit))?;
        for refused in &removal.kept {
            let path = refused.path.strip_prefix(root).unwrap_or(&refused.path);
            writeln!(out, "  {}  —  {}", path.display(), refused.reason)?;
        }
    }
    if !removal.failures.is_empty() {
        writeln!(
            out,
            "failed on {}, listed on standard error",
            plural(removal.failures.len(), PATH)
        )?;
    }
    Ok(())
}

/// Qualifies the numbers above when the scan could not read everything it was pointed at.
///
/// On stdout, beside the numbers it qualifies, because someone reading only the listing would
/// otherwise take an undercount for a total. The detail goes to standard error.
fn report_scan(out: &mut impl Write, outcome: &WalkOutcome) -> std::io::Result<()> {
    if outcome.errors.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "scan incomplete: {} could not be read, so everything above is a lower bound",
        plural(outcome.errors.len(), PATH),
    )?;
    for error in &outcome.errors {
        match &error.path {
            Some(path) => eprintln!("pristine: {}: {}", path.display(), error.message),
            None => eprintln!("pristine: {}", error.message),
        }
    }
    Ok(())
}

/// A noun and its plural, so a count can be read aloud.
type Noun = (&'static str, &'static str);

/// The sweep only ever claims directories.
const DIRECTORY: Noun = ("directory", "directories");
/// Repo mode takes whatever git lists, which is files as often as directories.
const PATH: Noun = ("path", "paths");
/// What git refuses to clean and this refuses to clean after it.
const NESTED_REPOSITORY: Noun = ("nested repository", "nested repositories");
/// What repo mode holds back unless `--node-modules` says otherwise.
const VENDORED: Noun = ("vendored path", "vendored paths");
/// What it holds back unless `--env` does.
const ENV_FILE: Noun = ("env file", "env files");
/// How many checkouts the fallback tier consulted.
const WORK_TREE: Noun = ("work tree", "work trees");

fn plural(count: usize, (one, many): Noun) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A claim's size, or a dash when nothing has looked. Not zero and not an error: measuring a
/// tier-one claim means enumerating the subtree the scan deliberately pruned at.
fn size(size: Size) -> String {
    match size {
        Size::Measured(bytes) => human(bytes),
        Size::Unmeasured => "—".to_owned(),
    }
}

/// Bytes in the units a person reads, binary because that is what the sizes are.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "a display rounded to one decimal place has none to lose"
    )]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A size as a person writes one.
///
/// Both readings of `MB` are in the wild, so neither is guessed at: the 1024-based units say
/// so (`M`, `MiB`) and the 1000-based ones say so (`MB`). Anything else is refused rather than
/// interpreted, because a floor that silently means something other than what was typed is a
/// floor that quietly claims directories the user meant to keep.
fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let digits = text
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .trim();
    let suffix = text[digits.len()..].trim();
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is not a whole number of bytes"))?;

    let multiplier: u64 = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KIB" => 1 << 10,
        "M" | "MIB" => 1 << 20,
        "G" | "GIB" => 1 << 30,
        "T" | "TIB" => 1 << 40,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "`{other}` is not a size suffix; use K, M, G or T (1024-based) or KB, MB, GB \
                 or TB (1000-based)"
            ));
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("`{text}` does not fit in a size"))
}

/// An age as a person writes one.
///
/// A unit is mandatory, because a bare number is the one input where guessing is worst: read
/// as seconds it keeps nothing, read as days it keeps almost everything, and both are
/// plausible readings of `--older-than 7`. There are deliberately no minutes or seconds — a
/// cleaner does not filter by them — which is what frees `m` to mean months without the
/// `m`-versus-`M` trap that would make the flag silently case-sensitive.
fn parse_duration(text: &str) -> Result<Duration, String> {
    const HOUR: u64 = 60 * 60;
    const DAY: u64 = 24 * HOUR;

    let text = text.trim();
    let digits = text
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .trim();
    let suffix = text[digits.len()..].trim();
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is not a whole number of time units"))?;

    let unit = match suffix.to_ascii_lowercase().as_str() {
        "h" => HOUR,
        "d" => DAY,
        "w" => 7 * DAY,
        "m" => 30 * DAY,
        "y" => 365 * DAY,
        "" => return Err(format!("`{text}` needs a unit: h, d, w, m or y")),
        other => {
            return Err(format!(
                "`{other}` is not a unit of time; use h, d, w, m or y"
            ));
        }
    };
    value
        .checked_mul(unit)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}` does not fit in a duration"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Mode, RepoArgs, Reset, anchor, ask, ask_reset, human, parse_duration, parse_size,
    };
    use clap::{CommandFactory, Parser};
    use pristine::{DEFAULT_MIN_SIZE, SizeMode};
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    /// The sweep's arguments off a parsed command line.
    fn sweep(args: &[&str]) -> super::Sweep {
        let mut line = vec!["pristine"];
        line.extend_from_slice(args);
        Cli::parse_from(line).sweep
    }

    /// Repo mode's arguments off a parsed command line.
    fn repo(args: &[&str]) -> RepoArgs {
        let mut line = vec!["pristine", "repo"];
        line.extend_from_slice(args);
        match Cli::parse_from(line).mode {
            Some(Mode::Repo(args)) => args,
            other => panic!("`repo` did not parse as repo mode: {other:?}"),
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_safe_defaults_are_the_defaults() {
        let cli = sweep(&[]);
        assert!(!cli.delete, "a bare run must not delete");
        assert!(!cli.yes, "consent is never assumed");
        assert!(cli.older_than.is_none(), "the age floor is opt-in");
        assert!(cli.one_file_system, "a mount is not crossed by default");
        assert!(!cli.breakdown, "a scan does not enumerate what it pruned");
        assert!(cli.breakdown_under.is_none());
    }

    #[test]
    fn a_mount_is_only_crossed_when_the_flag_says_so() {
        assert!(!sweep(&["--one-file-system=false"]).one_file_system);
        assert!(sweep(&["--one-file-system", "true"]).one_file_system);
    }

    #[test]
    fn the_floor_defaults_to_ten_mebibytes_and_the_flag_overrides_it() {
        assert_eq!(
            sweep(&[]).min_size,
            DEFAULT_MIN_SIZE,
            "10 MiB is the documented default"
        );
        assert_eq!(sweep(&["--min-size", "512K"]).min_size, 512 * 1024);
    }

    // --------------------------------------------------------------------------------------
    // Asking for sizes, which a scan does not volunteer.
    // --------------------------------------------------------------------------------------

    #[test]
    fn a_scan_prices_nothing_unless_it_is_asked_to() {
        // The default is the performance thesis: 4.6 s over one real ~/repos against 55.8 s
        // with a full breakdown. It stays the default, and the flag is how you leave it.
        assert_eq!(sweep(&[]).size_mode().unwrap(), SizeMode::Skip);
        assert_eq!(
            sweep(&["--breakdown"]).size_mode().unwrap(),
            SizeMode::Breakdown
        );
    }

    #[test]
    fn the_two_breakdown_flags_do_not_both_apply() {
        // "Price everything" and "price this subtree" are answers to the same question, and a
        // run that took both would silently honour one of them.
        assert!(
            Cli::try_parse_from(["pristine", "--breakdown", "--breakdown-under", "."]).is_err()
        );
    }

    #[test]
    fn a_scope_is_anchored_the_way_the_walk_spells_its_hits() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("app")).unwrap();

        // A hit carries the scan root exactly as it was typed, so the scope has to as well —
        // and the two spellings differ in both directions. On macOS a temporary directory is
        // reached through a symlink (`/var` → `/private/var`), so a resolved scope compared
        // against unresolved hits matches nothing. Silently, which is the whole danger.
        let resolved = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            anchor(tmp.path(), &resolved.join("app")).unwrap(),
            tmp.path().join("app")
        );

        // ...and the other way round: a root written with a `..` in it is what every hit is
        // prefixed with, so that is what the scope has to be prefixed with too.
        let awkward = tmp.path().join("app/..");
        assert_eq!(
            anchor(&awkward, &tmp.path().join("app")).unwrap(),
            awkward.join("app")
        );
    }

    #[test]
    fn a_scope_that_is_not_in_the_scan_is_refused_rather_than_pricing_nothing() {
        // Both of these would otherwise match no claim and produce a listing of dashes, which
        // reads exactly like "this subtree is worth nothing".
        let tmp = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();

        assert!(
            anchor(tmp.path(), elsewhere.path()).is_err(),
            "another tree"
        );
        assert!(
            anchor(tmp.path(), &tmp.path().join("typo")).is_err(),
            "a path that is not there"
        );
    }

    // --------------------------------------------------------------------------------------
    // Repo mode's command line.
    // --------------------------------------------------------------------------------------

    #[test]
    fn a_bare_run_asks_and_a_run_with_any_action_flag_does_not() {
        // The rule that keeps CI from hanging on a prompt, and it has to hold for every flag
        // that says anything about the plan — the two modifiers included, because passing one
        // is a statement even though it selects nothing on its own.
        assert!(!repo(&[]).chosen(), "a bare run has nothing to go on");
        for flag in [
            "--reset",
            "--untracked",
            "--ignored",
            "--node-modules",
            "--env",
            // `--yes` selects nothing, but it does mean the command line is resolving the
            // plan. Left out, it would put the cascade and the skipped final confirmation in
            // the same run.
            "--yes",
        ] {
            assert!(repo(&[flag]).chosen(), "`{flag}` left the run interactive");
        }
    }

    #[test]
    fn yes_gates_the_confirmation_and_selects_nothing() {
        // The distinction the design is explicit about: `pristine repo --ignored` in CI still
        // refuses to delete without `--yes`, and `--yes` on its own still removes nothing.
        let consented = repo(&["--yes"]);
        assert!(consented.yes);
        assert!(
            consented.selection().is_empty(),
            "consent was read as a selection"
        );
        // It does resolve the plan from the command line, though, which is a different claim
        // from selecting something and is what keeps the cascade and the skipped confirmation
        // out of the same run. See `RepoArgs::chosen`.
        assert!(consented.chosen(), "consent left the run interactive");
    }

    #[test]
    fn vendor_and_env_are_off_unless_the_flag_turns_them_on() {
        let asked_for_everything = repo(&["--untracked", "--ignored"]).selection();
        assert!(asked_for_everything.untracked && asked_for_everything.ignored);
        assert!(
            !asked_for_everything.vendor && !asked_for_everything.env,
            "asking for the lists was read as asking for what they hold back"
        );

        let opted_in = repo(&["--ignored", "--node-modules", "--env"]).selection();
        assert!(opted_in.vendor && opted_in.env);
        // ...and the value form turns them back off, which is what `--no-node-modules` was for.
        let opted_out = repo(&["--ignored", "--node-modules=false"]).selection();
        assert!(!opted_out.vendor);
    }

    #[test]
    fn a_bare_reset_is_a_hard_one_and_the_scope_is_spelled_out_otherwise() {
        assert_eq!(repo(&["--reset"]).selection().reset, Some(Reset::Hard));
        assert_eq!(repo(&["--reset=hard"]).selection().reset, Some(Reset::Hard));
        assert_eq!(
            repo(&["--reset=worktree"]).selection().reset,
            Some(Reset::WorkTree)
        );
        assert_eq!(repo(&[]).selection().reset, None);
        // `--reset` takes its value with `=` only, so it cannot swallow the flag after it.
        assert_eq!(
            repo(&["--reset", "--untracked"]).selection().reset,
            Some(Reset::Hard)
        );
        assert!(repo(&["--reset", "--untracked"]).untracked);
    }

    // --------------------------------------------------------------------------------------
    // Repo mode's prompts. Every one of them defaults to the answer that changes nothing.
    // --------------------------------------------------------------------------------------

    /// Runs the cascade over `answers` and returns what it decided and what it showed.
    fn asked(answers: &str) -> (pristine::Selection, String) {
        let mut shown = Vec::new();
        let selection = ask(&mut answers.as_bytes(), &mut shown).unwrap();
        (selection, String::from_utf8(shown).unwrap())
    }

    #[test]
    fn a_cascade_nobody_answers_selects_nothing() {
        // A pipe with nothing in it, and a person holding Enter. Neither is consent to
        // anything, so neither may reach the deleter.
        for silence in ["", "\n\n\n\n\n"] {
            assert!(
                asked(silence).0.is_empty(),
                "`{silence:?}` was read as a selection"
            );
        }
    }

    #[test]
    fn the_cascade_asks_about_vendor_and_env_only_once_a_list_was_taken() {
        let (_, untouched) = asked("\n\n\n");
        assert!(
            !untouched.contains("node_modules"),
            "it asked about what to hold back from a list nobody took:\n{untouched}"
        );

        let (selection, asked_about) = asked("1\ny\nn\ny\nn\n");
        assert!(selection.untracked && !selection.ignored);
        assert!(selection.vendor && !selection.env, "{selection:?}");
        // Asked even though only the untracked list was taken: the filters apply to both, so
        // an untracked-but-not-ignored node_modules is held back by the same rule.
        assert!(asked_about.contains("node_modules"), "{asked_about}");
    }

    #[test]
    fn only_a_number_that_names_a_reset_resets_anything() {
        for answer in ["2\n", "worktree\n"] {
            assert_eq!(
                ask_reset(&mut answer.as_bytes(), &mut Vec::new()).unwrap(),
                Some(Reset::WorkTree),
                "{answer:?}"
            );
        }
        for answer in ["3\n", "hard\n", "HARD\n"] {
            assert_eq!(
                ask_reset(&mut answer.as_bytes(), &mut Vec::new()).unwrap(),
                Some(Reset::Hard),
                "{answer:?}"
            );
        }
        // Only 2 and 3 discard anything, on the same principle as "only yes means yes": an
        // answer nobody can read is not an instruction to throw work away.
        for answer in ["", "\n", "1\n", "y\n", "yes\n", "4\n", "everything\n"] {
            assert_eq!(
                ask_reset(&mut answer.as_bytes(), &mut Vec::new()).unwrap(),
                None,
                "{answer:?}"
            );
        }
    }

    #[test]
    fn the_reset_menu_says_which_git_command_each_answer_runs() {
        let mut shown = Vec::new();
        ask_reset(&mut "\n".as_bytes(), &mut shown).unwrap();
        let shown = String::from_utf8(shown).unwrap();

        assert!(shown.contains("git restore -- ."), "{shown}");
        assert!(shown.contains("git reset --hard HEAD"), "{shown}");
        assert!(
            shown.contains("[1]"),
            "the default was not stated:\n{shown}"
        );
    }

    #[test]
    fn sizes_are_read_in_the_units_they_were_written_in() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("4096B"), Ok(4096));
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("1KiB"), Ok(1024));
        assert_eq!(parse_size("1MiB"), Ok(1024 * 1024));
        assert_eq!(parse_size("10 MiB"), Ok(DEFAULT_MIN_SIZE));
        assert_eq!(parse_size("2GiB"), Ok(2 * 1024 * 1024 * 1024));
        // Both readings of `MB` are in the wild, so the 1000-based spelling means 1000.
        assert_eq!(parse_size("1MB"), Ok(1_000_000));
        assert_eq!(parse_size("1kb"), Ok(1_000));
    }

    #[test]
    fn a_size_that_cannot_be_read_is_refused_rather_than_guessed_at() {
        // Silently taking any of these as some other number is how a floor stops meaning what
        // the user typed, and a floor that drifts claims directories they meant to keep.
        for bad in ["", "MiB", "1.5G", "-1", "1 potato", "18446744073709551615K"] {
            assert!(parse_size(bad).is_err(), "`{bad}` was accepted");
        }
    }

    #[test]
    fn an_age_is_read_in_the_units_it_was_written_in() {
        const DAY: u64 = 24 * 60 * 60;
        assert_eq!(parse_duration("12h"), Ok(Duration::from_secs(12 * 60 * 60)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * DAY)));
        assert_eq!(parse_duration("2w"), Ok(Duration::from_secs(14 * DAY)));
        assert_eq!(parse_duration("3m"), Ok(Duration::from_secs(90 * DAY)));
        assert_eq!(parse_duration("3M"), Ok(Duration::from_secs(90 * DAY)));
        assert_eq!(parse_duration("1y"), Ok(Duration::from_secs(365 * DAY)));
        assert_eq!(parse_duration(" 30 d "), Ok(Duration::from_secs(30 * DAY)));
    }

    #[test]
    fn an_age_that_cannot_be_read_is_refused_rather_than_guessed_at() {
        // A bare number is the worst one to guess at: as seconds it keeps nothing, as days
        // it keeps nearly everything, and `--older-than 7` reads as either.
        for bad in ["", "7", "d", "1.5d", "-7d", "7 potatoes", "7s", "7min"] {
            assert!(parse_duration(bad).is_err(), "`{bad}` was accepted");
        }
    }

    #[test]
    fn sizes_are_printed_in_the_units_a_person_reads() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(DEFAULT_MIN_SIZE), "10.0 MiB");
    }
}
