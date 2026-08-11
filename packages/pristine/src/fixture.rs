//! Claims made up out of thin air, for the tests that are about what happens *after* a walk.
//!
//! The rollup, the front end and the renderer all take a [`Hit`] and none of them cares where
//! it came from, so making one by hand is both faster than a temporary directory and able to
//! describe things a fixture on disk cannot — a claim nobody has priced, a directory stamped a
//! year ago. One builder rather than one per test module, because three copies of "what a hit
//! looks like" is three places to update when it grows a field.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::rules::Ruleset;
use crate::size::Size;
use crate::walk::{Claim, Hit, RuleClaim};

/// A tier-one claim at `path`, with a size and an mtime `seconds` after an arbitrary epoch.
///
/// The epoch is arbitrary on purpose: every test that cares about age is comparing two of
/// these against each other, and one anchored to the real clock would drift.
pub(crate) fn hit(path: &str, size: Size, seconds: u64) -> Hit {
    // Whichever rule is first. Nothing downstream of the walk reads the rule except to print
    // the command that regenerates the directory, so which one it is does not matter — only
    // that a tier-one claim has one and a tier-two claim does not.
    let ruleset = Ruleset::builtin().expect("the built-in ruleset parses");
    let rule = Arc::clone(&ruleset.rules()[0]);
    Hit {
        path: PathBuf::from(path),
        claim: Claim::Rule(RuleClaim {
            project_root: PathBuf::from("/scan"),
            regenerate: rule.regenerate.clone(),
            rule,
        }),
        size,
        modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
    }
}

/// A claim with a number on it.
pub(crate) fn priced(path: &str, bytes: u64) -> Hit {
    hit(path, Size::Measured(bytes), 0)
}
