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

use crate::rules::{Anchor, Kind, MarkersRequired, Rule, Ruleset};
use crate::size::Size;
use crate::walk::{Claim, Hit, IgnoredClaim, IgnoredFileClaim, RuleClaim};

/// A tier-one claim at `path`, with a size and an mtime `seconds` after an arbitrary epoch.
///
/// The epoch is arbitrary on purpose: every test that cares about age is comparing two of
/// these against each other, and one anchored to the real clock would drift.
pub(crate) fn hit(path: &str, size: Size, seconds: u64) -> Hit {
    // Whichever rule is first. Nothing downstream of the walk reads the rule except to print
    // what the directory is, so which one it is does not matter — only that a tier-one claim
    // has a rule and a tier-two claim does not.
    let ruleset = Ruleset::builtin().expect("the built-in ruleset parses");
    let rule = Arc::clone(&ruleset.rules()[0]);
    Hit {
        shared: 0,
        path: PathBuf::from(path),
        claim: Claim::Rule(RuleClaim {
            project_root: PathBuf::from("/scan"),
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

/// A tier-one claim of a named `kind`, for the tests about what a view shows.
///
/// The rule is made up rather than looked up, so a test that says "a cache" is not also
/// asserting that the shipped ruleset still happens to contain one.
pub(crate) fn of_kind(path: &str, kind: Kind) -> Hit {
    let mut made = hit(path, Size::Measured(1), 0);
    made.claim = Claim::Rule(RuleClaim {
        project_root: PathBuf::from("/scan"),
        rule: Arc::new(Rule {
            id: format!("test-{kind}"),
            ecosystem: "Test".to_owned(),
            kind,
            markers: vec!["marker".to_owned()],
            markers_required: MarkersRequired::Any,
            targets: vec!["target".to_owned()],
            anchor: Anchor::Parent,
            note: None,
        }),
    });
    made
}

/// A tier-two claim: one only the gitignore fallback found, so nothing knows what it is.
pub(crate) fn gitignored(path: &str) -> Hit {
    let mut made = hit(path, Size::Measured(1), 0);
    made.claim = Claim::Ignored(IgnoredClaim {
        work_tree: PathBuf::from("/scan"),
    });
    made
}

/// A tier-two claim on a **file**, of whatever `kind` its name would have said.
///
/// Always priced, because a real one always is: one `lstat` is the complete answer for a leaf,
/// so a fixture that could make an unpriced file claim would be able to describe a state the
/// walk cannot produce.
pub(crate) fn gitignored_file(path: &str, kind: Option<Kind>) -> Hit {
    let mut made = hit(path, Size::Measured(1), 0);
    made.claim = Claim::IgnoredFile(IgnoredFileClaim {
        work_tree: PathBuf::from("/scan"),
        kind,
    });
    made
}
