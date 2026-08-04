//! A dumb, token-free daemon that decides when to run the executor command.
//!
//! Its complete read surface is three things: whether the remote log head
//! moved, the loop fold, and the wake deadline the last pass asked for. It
//! exercises no judgment about work; deciding what to do is the executor's
//! job, and *how* the executor runs — sessions, engines, panes — is the
//! configured command's business, not this daemon's.
//!
//! Decisions live in [`decide`] as pure functions over a snapshot, and every
//! effect goes through [`effects::Effects`], so the interesting behaviour is
//! testable without a shell or Git.

pub mod config;
pub mod decide;
pub mod driver;
pub mod effects;
pub mod error;
pub mod loop_state;
