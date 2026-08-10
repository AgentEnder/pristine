//! `pristine` finds reclaimable build artifacts and vendored dependency directories across
//! every ecosystem on a machine, and tells you what regenerates each one before you delete it.
//!
//! The command line here is deliberately thin — the rollup tree TUI (#602) is the real front
//! end — but three of the safety model's promises live nowhere else, because they are
//! properties of the *program* rather than of the library:
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
//! all of them, which is why every guard above matters more here than it eventually will.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use clap::{ArgAction, Parser};
use pristine::delete::confirm;
use pristine::{Deleter, Hit, Plan, Planner, Removal, Ruleset, Size, Target, Walker};

/// A language-agnostic reclaimable-space finder and cleaner.
#[derive(Debug, Parser)]
#[command(name = "pristine", version)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, and the lint's advice — fold them into a state \
              machine — would take the flags off the command line"
)]
struct Cli {
    /// The directory to scan.
    #[arg(default_value = ".", value_name = "PATH")]
    root: PathBuf,

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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        // Anything the run could not do — a path it could not read, a directory it could not
        // remove — is a lower bound reported as a total unless the status says otherwise.
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("pristine: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether everything asked for actually happened.
fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error>> {
    let ruleset = Arc::new(Ruleset::load(None)?);
    let hits = Mutex::new(Vec::new());
    // The flag has to reach the walk as well as the plan. Setting it on only one of them
    // makes `--one-file-system=false` a flag that permits crossing a mount the scan never
    // looked across, which reads as "there was nothing over there".
    let outcome = Walker::new(&cli.root, Arc::clone(&ruleset))
        .same_file_system(cli.one_file_system)
        .run(|hit| lock(&hits).push(hit));

    let mut hits = hits.into_inner().unwrap_or_else(PoisonError::into_inner);
    // Biggest first, and the unpriced after the priced rather than ahead of them: a claim
    // has no number because nothing looked, not because it is empty.
    hits.sort_by(|a, b| {
        b.size
            .bytes()
            .cmp(&a.size.bytes())
            .then_with(|| a.path.cmp(&b.path))
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let whole = outcome.errors.is_empty();

    if !cli.delete && !cli.dry_run {
        for hit in &hits {
            writeln!(out, "{}", row(hit, &cli.root))?;
        }
        writeln!(
            out,
            "\n{} reclaimable",
            plural(hits.len(), "directory", "directories")
        )?;
        report_scan(&mut out, &outcome)?;
        return Ok(whole);
    }

    let plan = Planner::new(&cli.root)
        .one_file_system(cli.one_file_system)
        .older_than(cli.older_than)
        .plan(hits.iter().map(Target::from));
    write_plan(&mut out, &plan)?;
    report_scan(&mut out, &outcome)?;

    if cli.dry_run {
        writeln!(out, "\ndry run: nothing was removed")?;
        return Ok(whole);
    }
    if plan.is_empty() {
        writeln!(out, "\nnothing was removed")?;
        return Ok(whole);
    }
    if !cli.yes {
        let question = format!(
            "\nRemove {}?",
            plural(plan.targets().len(), "directory", "directories")
        );
        let stdin = std::io::stdin();
        if !confirm(&question, &mut stdin.lock(), &mut out)? {
            writeln!(out, "nothing was removed")?;
            return Ok(whole);
        }
    }

    let removal = Deleter::new().remove(&plan);
    write_removal(&mut out, &removal, plan.root())?;
    for failure in &removal.failures {
        eprintln!("pristine: {}: {}", failure.path.display(), failure.message);
    }
    Ok(whole && removal.is_clean())
}

/// One reclaimable directory, with what is known about getting it back.
fn row(hit: &Hit, root: &Path) -> String {
    let path = hit.path.strip_prefix(root).unwrap_or(&hit.path);
    format!(
        "{:>10}  {:<60}  {}",
        size(hit.size),
        path.display(),
        hit.regenerate
    )
}

/// The plan, in full. This is what `--dry-run` exists to show, so it lists what would be
/// removed AND what would not: a plan that printed only the first half would be
/// indistinguishable from a clean machine when an age floor or a checkout kept everything.
fn write_plan(out: &mut impl Write, plan: &Plan) -> std::io::Result<()> {
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
        plural(plan.targets().len(), "directory", "directories"),
        human(plan.measured_bytes()),
        plan.unpriced(),
    )?;

    if plan.kept().is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "kept: {}",
        plural(plan.kept().len(), "directory", "directories")
    )?;
    for refused in plan.kept() {
        let path = refused.path.strip_prefix(root).unwrap_or(&refused.path);
        writeln!(out, "  {}  —  {}", path.display(), refused.reason)?;
    }
    Ok(())
}

fn write_removal(out: &mut impl Write, removal: &Removal, root: &Path) -> std::io::Result<()> {
    let complete = removal
        .removed
        .iter()
        .filter(|removed| removed.complete)
        .count();
    writeln!(
        out,
        "\nremoved {}, {} freed",
        plural(complete, "directory", "directories"),
        human(removal.bytes_freed()),
    )?;

    // A subtree the deleter declined to enter is the safety model working rather than a
    // fault, so it is reported here and not as a failure — but it IS reported, because a
    // directory the user selected and did not get is something they need to know about.
    if !removal.kept.is_empty() {
        writeln!(
            out,
            "kept {}:",
            plural(removal.kept.len(), "directory", "directories")
        )?;
        for refused in &removal.kept {
            let path = refused.path.strip_prefix(root).unwrap_or(&refused.path);
            writeln!(out, "  {}  —  {}", path.display(), refused.reason)?;
        }
    }
    if !removal.failures.is_empty() {
        writeln!(
            out,
            "failed on {}, listed on standard error",
            plural(removal.failures.len(), "path", "paths")
        )?;
    }
    Ok(())
}

/// Qualifies the numbers above when the scan could not read everything it was pointed at.
fn report_scan(out: &mut impl Write, outcome: &pristine::WalkOutcome) -> std::io::Result<()> {
    if outcome.errors.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "scan incomplete: {} could not be read, so everything above is a lower bound",
        plural(outcome.errors.len(), "path", "paths"),
    )?;
    for error in &outcome.errors {
        match &error.path {
            Some(path) => eprintln!("pristine: {}: {}", path.display(), error.message),
            None => eprintln!("pristine: {}", error.message),
        }
    }
    Ok(())
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A claim's size, or a dash when nothing has looked. Not zero and not an error: measuring
/// means enumerating the subtree the scan deliberately pruned at.
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
    use super::{Cli, human, parse_duration};
    use clap::{CommandFactory, Parser};
    use std::time::Duration;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_safe_defaults_are_the_defaults() {
        let cli = Cli::parse_from(["pristine"]);
        assert!(!cli.delete, "a bare run must not delete");
        assert!(!cli.yes, "consent is never assumed");
        assert!(cli.older_than.is_none(), "the age floor is opt-in");
        assert!(cli.one_file_system, "a mount is not crossed by default");
    }

    #[test]
    fn a_mount_is_only_crossed_when_the_flag_says_so() {
        assert!(!Cli::parse_from(["pristine", "--one-file-system=false"]).one_file_system);
        assert!(Cli::parse_from(["pristine", "--one-file-system", "true"]).one_file_system);
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
        assert_eq!(human(10 * 1024 * 1024), "10.0 MiB");
    }
}
