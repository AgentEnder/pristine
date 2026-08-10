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
//! println!("{} reclaimable in {} directories", tree.reclaimable(), outcome.hits);
//! # Ok(())
//! # }
//! ```
//!
//! Detection is marker-anchored and never name-anchored: a rule is "a directory named
//! `target` whose parent holds a `Cargo.toml`", never "a directory named `target`".
//! `target` is Rust's and Maven's, `vendor` is Go's and Composer's and Bundler's, and `build`
//! is Gradle's, Dart's and — in a CMake project — hand-written source. See [`rules`].

mod detect;
pub mod rules;
pub mod size;
pub mod tree;
pub mod walk;

pub use rules::{Anchor, MarkersRequired, Rule, RuleError, Ruleset};
pub use size::{Measurement, Measurer, SizeMode};
pub use tree::{Node, NodeId, Tree};
pub use walk::{Hit, WalkError, WalkOutcome, Walker};
