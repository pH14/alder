//! The observation application.
//!
//! Observations are current levels reported by dumb external observers —
//! `(observer, subject, field) = level` — folded from durable
//! `observation.*` events. This crate owns that schema, the validation of
//! its keys and reports, its fold, and the newness check that keeps a
//! repeated report from becoming a second event. It knows nothing about
//! work, attempts, or the loop: the log is beneath it, and composition with
//! the other application happens above it.

mod model;
mod observer;
mod state;

pub use model::*;
pub use observer::*;
pub use state::*;
