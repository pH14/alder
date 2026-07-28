use std::sync::Mutex;

use serde_json::json;

use crate::{
    domain::{Event, EventDraft, Head},
    error::{AlderError, Result},
};

use super::{AppendResult, Store};

#[derive(Debug, Default)]
pub struct MemoryStore {
    events: Mutex<Vec<Event>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemoryStore {
    fn current_head(&self) -> Result<Head> {
        let events = self.events.lock().expect("memory store mutex poisoned");
        Ok(memory_head(events.len()))
    }

    fn read_events(&self, head: &Head) -> Result<Vec<Event>> {
        let events = self.events.lock().expect("memory store mutex poisoned");
        if head.seq > events.len() as u64 {
            return Err(AlderError::with_context(
                "invalid_head",
                "the requested memory head does not exist",
                json!({"head": head}),
            ));
        }
        Ok(events[..head.seq as usize].to_vec())
    }

    fn append(&self, expected: &Head, draft: &EventDraft) -> Result<AppendResult> {
        let mut events = self.events.lock().expect("memory store mutex poisoned");
        if let Some(existing) = events.iter().find(|event| event.id == draft.id) {
            return Ok(AppendResult {
                head: memory_head(existing.seq as usize),
                event: existing.clone(),
            });
        }
        let current = memory_head(events.len());
        if &current != expected {
            return Err(AlderError::with_context(
                "head_conflict",
                "the shared log advanced before the event was appended",
                json!({"expected_head": expected, "current_head": current}),
            ));
        }
        let event = draft.materialize(expected.seq + 1);
        events.push(event.clone());
        Ok(AppendResult {
            head: memory_head(events.len()),
            event,
        })
    }
}

fn memory_head(length: usize) -> Head {
    Head {
        revision: (length > 0).then(|| format!("memory-{length}")),
        seq: length as u64,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::domain::{EventDraft, EventPayload};

    fn draft(id: &str) -> EventDraft {
        EventDraft {
            id: id.to_owned(),
            at: Utc::now(),
            actor: "test".to_owned(),
            payload: EventPayload::WorkChanged {
                why: None,
                operations: vec![],
            },
            schema: "alder.event.v0".to_owned(),
        }
    }

    #[test]
    fn compare_and_swap_allows_only_one_writer() {
        let store = MemoryStore::new();
        let head = store.current_head().unwrap();
        store.append(&head, &draft("one")).unwrap();
        let error = store.append(&head, &draft("two")).unwrap_err();
        assert_eq!(error.code, "head_conflict");
        assert_eq!(store.current_head().unwrap().seq, 1);
    }

    #[test]
    fn event_id_is_idempotent() {
        let store = MemoryStore::new();
        let head = store.current_head().unwrap();
        let first = store.append(&head, &draft("same")).unwrap();
        let second = store.append(&head, &draft("same")).unwrap();
        assert_eq!(first.event.seq, second.event.seq);
        assert_eq!(store.current_head().unwrap().seq, 1);
    }

    #[test]
    fn heads_and_prefix_reads_preserve_exact_boundaries() {
        let store = MemoryStore::new();
        assert_eq!(store.current_head().unwrap(), Head::empty());

        let empty = store.current_head().unwrap();
        let first = store.append(&empty, &draft("one")).unwrap();
        let second = store.append(&first.head, &draft("two")).unwrap();

        assert_eq!(
            first.head,
            Head {
                revision: Some("memory-1".to_owned()),
                seq: 1,
            }
        );
        assert_eq!(
            second.head,
            Head {
                revision: Some("memory-2".to_owned()),
                seq: 2,
            }
        );
        assert!(store.read_events(&Head::empty()).unwrap().is_empty());
        assert_eq!(store.read_events(&first.head).unwrap().len(), 1);
        assert_eq!(store.read_events(&second.head).unwrap().len(), 2);

        let error = store
            .read_events(&Head {
                revision: Some("memory-3".to_owned()),
                seq: 3,
            })
            .unwrap_err();
        assert_eq!(error.code, "invalid_head");
    }
}
