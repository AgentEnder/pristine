//! `pristine` finds reclaimable build artifacts and vendored dependency directories across
//! every ecosystem on a machine, and tells you what regenerates each one before you delete it.
//!
//! The command line here is deliberately thin. The rollup tree TUI (#602) is the real front
//! end and the deleter (#594) is what makes this a cleaner rather than a finder; what follows
//! is enough to point the library at a directory and read what it found, and it exists mainly
//! so `--min-size` is a flag a person can type rather than a builder method.
//!
//! Two things it does have to get right, because they are properties of the *output* rather
//! than of the scan: a tier-two hit says it does not know how to regenerate what it found, and
//! a tier that could not run says so instead of looking like a clean result.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser;
use pristine::{DEFAULT_MIN_SIZE, Hit, Ruleset, Size, Walker};

/// A language-agnostic reclaimable-space finder and cleaner.
#[derive(Debug, Parser)]
#[command(name = "pristine", version)]
struct Cli {
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match scan(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("pristine: {err}");
            ExitCode::FAILURE
        }
    }
}

fn scan(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let ruleset = Arc::new(Ruleset::load(None)?);
    let hits = Mutex::new(Vec::new());
    let outcome = Walker::new(&cli.root, ruleset)
        .min_size(cli.min_size)
        .run(|hit| lock(&hits).push(hit));

    let mut hits = hits.into_inner().unwrap_or_else(PoisonError::into_inner);
    // Biggest first, and the unpriced after the priced rather than ahead of them: a tier-one
    // claim has no number because nothing looked, not because it is empty.
    hits.sort_by(|a, b| {
        b.size
            .bytes()
            .cmp(&a.size.bytes())
            .then_with(|| a.path.cmp(&b.path))
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for hit in &hits {
        writeln!(out, "{}", row(hit, &cli.root))?;
    }
    writeln!(out, "{}", summary(&hits, &outcome))?;

    for error in &outcome.errors {
        match &error.path {
            Some(path) => eprintln!("pristine: {}: {}", path.display(), error.message),
            None => eprintln!("pristine: {}", error.message),
        }
    }
    Ok(())
}

/// One reclaimable directory, with what is known about getting it back.
fn row(hit: &Hit, root: &Path) -> String {
    let path = hit.path.strip_prefix(root).unwrap_or(&hit.path);
    let size = match hit.size {
        Size::Measured(bytes) => human(bytes),
        // Not zero and not an error. Nothing has looked inside a tier-one claim, because
        // looking is the cost the whole design exists to avoid.
        Size::Unmeasured => "—".to_owned(),
    };
    let regenerate = hit.regenerate().unwrap_or(
        // The asymmetry is the point: this tier knows the directory is safe to remove and
        // knows nothing about what put it there, which tells you the deletion is not cheap.
        "no known way to regenerate this",
    );
    format!("{size:>10}  {:<60}  {regenerate}", path.display())
}

fn summary(hits: &[Hit], outcome: &pristine::WalkOutcome) -> String {
    let priced: u64 = hits.iter().filter_map(|hit| hit.size.bytes()).sum();
    let unpriced = hits.iter().filter(|hit| hit.size.bytes().is_none()).count();
    let mut lines = vec![format!(
        "\n{} reclaimable, {} priced, {unpriced} not priced",
        plural(hits.len(), "directory", "directories"),
        human(priced),
    )];

    let fallback = &outcome.fallback;
    if !fallback.enabled {
        return lines.join("\n");
    }
    if fallback.is_inert() {
        // The honest report. Silence here would be indistinguishable from a clean scan, and
        // the two mean opposite things.
        lines.push(format!(
            "fallback tier: inert — nothing scanned is in a git work tree, so nothing could be \
             judged reclaimable by inference (floor was {})",
            human(fallback.min_size)
        ));
        return lines.join("\n");
    }
    let held_back = if fallback.holding_a_checkout == 0 {
        String::new()
    } else {
        format!(
            "; {} left alone because they hold a checkout",
            plural(fallback.holding_a_checkout, "directory", "directories")
        )
    };
    lines.push(format!(
        "fallback tier: {} found in {} above a {} floor{held_back}",
        plural(fallback.hits, "directory", "directories"),
        plural(fallback.work_trees, "work tree", "work trees"),
        human(fallback.min_size),
    ));
    lines.join("\n")
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{Cli, human, parse_size};
    use clap::{CommandFactory, Parser};
    use pristine::DEFAULT_MIN_SIZE;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_floor_defaults_to_ten_mebibytes_and_the_flag_overrides_it() {
        assert_eq!(
            Cli::parse_from(["pristine"]).min_size,
            DEFAULT_MIN_SIZE,
            "10 MiB is the documented default"
        );
        assert_eq!(
            Cli::parse_from(["pristine", "--min-size", "512K"]).min_size,
            512 * 1024
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
    fn sizes_are_printed_in_the_units_a_person_reads() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(DEFAULT_MIN_SIZE), "10.0 MiB");
    }
}
