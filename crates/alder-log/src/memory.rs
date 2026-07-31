use crate::{AppendDisposition, AppendReceipt, Head, Log, LogError, Record, RecordDraft};
use std::sync::Mutex;

/// An in-process log implementation, useful for tests and embedding.
#[derive(Debug, Default)]
pub struct MemoryLog {
    records: Mutex<Vec<Record>>,
}
impl MemoryLog {
    /// Create an empty in-memory log.
    pub fn new() -> Self {
        Self::default()
    }
}
impl Log for MemoryLog {
    fn head(&self) -> Result<Head, LogError> {
        let records = self.records.lock().expect("memory log mutex poisoned");
        memory_head(records.len())
    }
    fn read(&self, through: &Head, after: u64) -> Result<Vec<Record>, LogError> {
        if after > through.sequence() {
            return Err(LogError::InvalidRange {
                after,
                through: through.sequence(),
            });
        }
        let records = self.records.lock().expect("memory log mutex poisoned");
        if through.sequence() > records.len() as u64
            || &memory_head(through.sequence() as usize)? != through
        {
            return Err(LogError::InvalidHead {
                message: "the requested memory head does not exist".to_owned(),
            });
        }
        Ok(records[after as usize..through.sequence() as usize].to_vec())
    }
    fn append(&self, expected: &Head, draft: &RecordDraft) -> Result<AppendReceipt, LogError> {
        let mut records = self.records.lock().expect("memory log mutex poisoned");
        let observed = memory_head(records.len())?;
        if let Some(existing) = records.iter().find(|record| record.id() == draft.id()) {
            if !existing.matches_draft(draft) {
                return Err(LogError::RecordIdCollision {
                    id: draft.id().clone(),
                });
            }
            return Ok(AppendReceipt {
                disposition: AppendDisposition::AlreadyPresent,
                record: existing.clone(),
                observed_head: observed,
            });
        }
        if expected != &observed {
            return Err(LogError::HeadConflict {
                expected: expected.clone(),
                observed,
            });
        }
        let record = Record::materialize(draft, observed.sequence() + 1);
        records.push(record.clone());
        Ok(AppendReceipt {
            disposition: AppendDisposition::Appended,
            record,
            observed_head: memory_head(records.len())?,
        })
    }
}
fn memory_head(length: usize) -> Result<Head, LogError> {
    if length == 0 {
        Ok(Head::empty())
    } else {
        Head::try_from_parts(length as u64, Some(format!("memory-{length}")))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::{EventType, RecordId, SchemaId};

    fn draft() -> RecordDraft {
        RecordDraft::new(
            RecordId::new("one").unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0).unwrap(),
            "test",
            EventType::new("example.changed").unwrap(),
            json!({}),
            SchemaId::new("example.v1").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn reads_allow_the_head_as_an_exclusive_after_bound() {
        let log = MemoryLog::new();
        let head = log.append(&Head::empty(), &draft()).unwrap().observed_head;
        assert!(log.read(&head, head.sequence()).unwrap().is_empty());
    }

    #[test]
    fn reads_reject_heads_that_are_too_new_or_name_the_wrong_revision() {
        let log = MemoryLog::new();
        log.append(&Head::empty(), &draft()).unwrap();
        for forged in [
            Head::try_from_parts(2, Some("memory-2".to_owned())).unwrap(),
            Head::try_from_parts(1, Some("not-memory-1".to_owned())).unwrap(),
        ] {
            assert!(matches!(
                log.read(&forged, 0),
                Err(LogError::InvalidHead { .. })
            ));
        }
    }
}
