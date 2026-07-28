//! A synchronous, append-only log of opaque JSON records.
//!
//! Git's configured remote reference is authoritative. SQLite projections are
//! intentionally outside this crate and may be rebuilt from these opaque records.
//! Expected-head append is the linearization point; applications must reread and
//! reconsider after a head conflict. Stable record IDs resolve lost responses.
//! The API is synchronous and the Git implementation invokes the `git` executable.

mod error;
mod git;
mod memory;
mod record;

pub use error::LogError;
pub use git::GitLog;
pub use memory::MemoryLog;
pub use record::{EventType, Head, Record, RecordDraft, RecordId, SchemaId};

/// Result returned by a successful append.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendReceipt {
    /// Whether this call created a record or found a matching prior record.
    pub disposition: AppendDisposition,
    /// The persisted record.
    pub record: Record,
    /// The newest complete head observed while resolving the call.
    pub observed_head: Head,
}

/// A successful append disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendDisposition {
    /// A new record was appended.
    Appended,
    /// The same record was already present.
    AlreadyPresent,
}

/// A synchronous, compare-and-append record log.
pub trait Log: Send + Sync {
    /// Return the current authoritative head.
    fn head(&self) -> Result<Head, LogError>;
    /// Read records after `after` through the supplied, pinned head.
    fn read(&self, through: &Head, after: u64) -> Result<Vec<Record>, LogError>;
    /// Compare the authoritative head and append exactly one record.
    ///
    /// Resolution follows this order:
    ///
    /// 1. If `draft.id()` already exists with identical timestamp, actor, event
    ///    type, body, and schema, return [`AppendDisposition::AlreadyPresent`].
    ///    The stored sequence is deliberately ignored during this comparison.
    /// 2. If the ID exists with different content, return
    ///    [`LogError::RecordIdCollision`].
    /// 3. Only for an absent ID, compare `expected` with the current head and
    ///    return [`LogError::HeadConflict`] when they differ.
    /// 4. Otherwise append exactly one sequenced record and return
    ///    [`AppendDisposition::Appended`].
    ///
    /// Idempotency therefore takes precedence over the expected-head check, so
    /// replaying the same draft after later appends succeeds. Every successful
    /// receipt's `observed_head` is the latest complete head observed while
    /// resolving that call. A head conflict is not retried here: the application
    /// must reread state and reconsider its decision.
    fn append(&self, expected: &Head, draft: &RecordDraft) -> Result<AppendReceipt, LogError>;
    /// Read every record through a supplied head.
    fn read_all(&self, through: &Head) -> Result<Vec<Record>, LogError> {
        self.read(through, 0)
    }
}
