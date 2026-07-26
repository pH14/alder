mod git;
mod memory;

use crate::{
    domain::{Event, EventDraft, Head},
    error::Result,
};

pub use git::GitStore;
pub use memory::MemoryStore;

#[derive(Debug, Clone)]
pub struct AppendResult {
    pub head: Head,
    pub event: Event,
}

pub trait Store {
    fn current_head(&self) -> Result<Head>;
    fn read_events(&self, head: &Head) -> Result<Vec<Event>>;
    fn append(&self, expected: &Head, event: &EventDraft) -> Result<AppendResult>;
}
