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
//! A curated ruleset only ever covers the ecosystems somebody wrote a rule for, so there is a
//! second tier underneath it: inside a git work tree, a directory that is gitignored, holds no
//! tracked file and no git checkout at any depth, and clears a size floor is reclaimable by
//! inference even when no rule names it. It reports honestly that it does not know how to
//! regenerate what it found, and outside a work tree it is inert rather than guessing from
//! directory names. See [`fallback`].

mod detect;
pub mod fallback;
pub mod git;
pub mod rules;
pub mod size;
pub mod tree;
pub mod walk;

pub use fallback::{DEFAULT_MIN_SIZE, FallbackReport};
pub use git::{GitError, WorkTree};
pub use rules::{Anchor, MarkersRequired, RegenerateWhen, Rule, RuleError, Ruleset};
pub use size::{Measurement, Measurer, Size, SizeMode, Survey};
pub use tree::{Node, NodeId, Tree};
pub use walk::{Claim, Hit, IgnoredClaim, RuleClaim, WalkError, WalkOutcome, Walker};
