//! Give a prompt to a model at some effort that runs somewhere; get a handle.
//!
//! The runner is one binary with four verbs:
//!
//! - `start` launches an execution: a worktree on the given branch, a tmux
//!   session running the tier's engine with the prompt file's contents as its
//!   final argument, and a printed handle;
//! - `status <handle>` answers `running`, `done`, or `dead`;
//! - `send <handle> --file <path>` delivers the file's contents as input to
//!   the running execution;
//! - `kill <handle>` ends it.
//!
//! The result's location is the branch given at start; the runner never
//! interprets it. The runner knows nothing about any caller's domain — see
//! `tests/boundary.rs` for the check that keeps it that way — and this crate
//! is deliberately movable to its own repository: nothing in this workspace
//! depends on it, and it depends on nothing in this workspace.

pub mod budget;
pub mod config;
pub mod error;
pub mod host;
pub mod limits;
pub mod ops;
pub mod start;
pub mod tier;
