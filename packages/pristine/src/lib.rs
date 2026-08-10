//! `pristine` finds reclaimable build artifacts and vendored dependency directories across
//! every ecosystem on a machine, and tells you what regenerates each one before you delete it.
//!
//! The core is a single parallel walk that prunes at every directory it claims, driven by a
//! ruleset that lives in TOML rather than in code:
//!
//! ```no_run
//! use std::sync::Arc;
//! use pristine::{Ruleset, Walker};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let ruleset = Arc::new(Ruleset::load(None)?);
//! let (tree, outcome) = Walker::new("/Users/me/repos", ruleset).run_to_tree();
//! println!("{} directories, {} not yet priced", outcome.hits, tree.unmeasured());
//! # Ok(())
//! # }
//! ```
//!
//! A scan does not measure what it claims. Pruning at `node_modules` and then walking it to
//! size it would give back the cost the pruning saved, so sizes arrive as
//! [`Size::Unmeasured`] until a caller asks for a breakdown with
//! [`SizeMode::Breakdown`](size::SizeMode::Breakdown).
//!
//! Detection is marker-anchored and never name-anchored: a rule is "a directory named
//! `target` whose parent holds a `Cargo.toml`", never "a directory named `target`".
//! `target` is Rust's and Maven's, `vendor` is Go's and Composer's and Bundler's, and `build`
//! is Gradle's, Dart's and — in a CMake project — hand-written source. See [`rules`].
//!
//! Removing what a scan found is [`delete`], and it is split in two on purpose. A [`Planner`]
//! resolves every path and applies every check in the safety model; a [`Deleter`] executes
//! the resulting [`Plan`] and decides nothing. That split is what makes a dry run honest: the
//! plan a preview prints is the same object the removal consumes.

pub mod delete;
mod detect;
pub mod rules;
pub mod size;
pub mod tree;
pub mod walk;

pub use delete::{
    Deleter, Failure, Plan, PlanTarget, Planner, Refusal, Refused, Removal, Removed, Target,
};
pub use rules::{Anchor, MarkersRequired, RegenerateWhen, Rule, RuleError, Ruleset};
pub use size::{Measurement, Measurer, Size, SizeMode};
pub use tree::{Node, NodeId, Tree};
pub use walk::{Hit, WalkError, WalkOutcome, Walker};
