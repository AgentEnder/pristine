//! `pristine` finds reclaimable build artifacts and vendored dependency directories across
//! every ecosystem on a machine, and tells you what regenerates each one before you delete it.
//!
//! Nothing is implemented yet beyond argument parsing. This crate is the scaffold that the
//! walker, the ruleset and the TUI hang off.

use clap::Parser;

/// A language-agnostic reclaimable-space finder and cleaner.
#[derive(Debug, Parser)]
#[command(name = "pristine", version)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
