//! The work application.
//!
//! Work items, their dependency graph and acceptance checks, the attempts
//! that execute them, and the questions that block them — the schema of the
//! `work.*`, `attempt.*`, `question.*`, and decode-only `handoff.*` events,
//! the legality checking of every such write, and the fold of that history
//! into current state. It knows nothing about observations or the loop: the
//! log is beneath it, and composition with the other application happens
//! above it.

mod change;
mod model;
mod state;

pub use change::*;
pub use model::*;
pub use state::*;
