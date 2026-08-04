//! A dumb, token-free daemon that decides when to wake an Alder executor agent.
//!
//! Its complete read surface is four things: whether the remote log head
//! moved, whether `alder refresh` saw a semantic observation change, the loop
//! fold, and the wake deadline the last pass asked for. It exercises no
//! judgment about work; deciding what to do is the executor's job.
//!
//! Decisions live in [`decide`] as pure functions over a snapshot, and every
//! effect goes through [`effects::Effects`], so the interesting behaviour is
//! testable without tmux or Git.

pub mod budget;
pub mod config;
pub mod decide;
pub mod driver;
pub mod effects;
pub mod error;
pub mod limits;
pub mod loop_state;
pub mod spawn;
pub mod tier;
