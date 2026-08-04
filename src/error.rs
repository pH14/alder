//! The shared error envelope, re-exported from the log crate.
//!
//! The type moved to `alder_log::alder_error` so the application crates can
//! speak it without depending on this crate; every `crate::error` import in
//! the CLI keeps working through this re-export.

pub use alder_log::alder_error::{AlderError, Result};
