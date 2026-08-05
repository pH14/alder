use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{DriverError, Result};

/// The driver's read of the loop section of `alder status --json`, plus the
/// head that document reported. This is the driver's complete view of the
/// durable log: it never reads work, attempts, or questions, because deciding
/// anything about them would be judgment.
///
/// Only the fields the driver's own triggers consume are parsed. The loop
/// section carries more — the desired engine, the rotation request — but
/// those belong to the configured command now, which reads status itself;
/// parsing them here would be knowledge without a use.
///
/// Everything here is a durable statement about the loop. Nothing records
/// whether this driver has acted on any of it — the log never mentions its
/// readers — so "already handled" is decided by comparing these fields with
/// the driver's own machine-local [`Notes`](crate::decide::Notes).
///
/// `PartialEq` so a test can compare two of these whole rather than field by
/// field: the simulator's answer against production's, both read back through
/// this type. A field added below then joins that comparison for free, which a
/// hand-written list of assertions would not.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct LoopState {
    /// The current log head. Compared with the last head the driver acted on,
    /// this is the whole wake rule. Not read from the loop section (the head
    /// sits at the document's top level); [`LoopState::from_status`] requires
    /// it explicitly.
    #[serde(default)]
    pub head: u64,
    /// Required, never defaulted: a loop section that does not say whether
    /// the loop is paused is a malformed document, and reading it as
    /// "unpaused" would let a truncated status run the command over a pause.
    pub paused: bool,
    /// The sequence of the latest nudge request, if any was ever made.
    #[serde(default)]
    pub nudge_requested_seq: Option<u64>,
    /// Every blocked item's `work block --until` deadline, sorted. The due
    /// trigger checks each one: an item still blocked past its own deadline
    /// must not swallow the wake a later deadline is owed.
    #[serde(default)]
    pub review_deadlines: Vec<DateTime<Utc>>,
}

/// The code every malformed-status error carries. The driver counts these
/// toward its outage notice: a store that answers with a document the wake
/// rule cannot be decided from is as unavailable as one that does not answer.
pub const MALFORMED_STATUS: &str = "malformed_status";

impl LoopState {
    /// Extract the loop section and the head, ignoring everything else
    /// `status` reports.
    ///
    /// Fail closed on partial documents: the head and the loop section's
    /// `paused` are required explicitly. A document missing either is an
    /// error — never a default — because "unpaused at head 0" read off a
    /// truncated status is indistinguishable from a real decision input.
    pub fn from_status(status: &Value) -> Result<Self> {
        let section = status.get("loop").ok_or_else(|| {
            DriverError::coded(
                MALFORMED_STATUS,
                "`alder status --json` has no loop section",
            )
        })?;
        let mut state: Self = serde_json::from_value(section.clone()).map_err(|error| {
            DriverError::coded(
                MALFORMED_STATUS,
                format!("unreadable loop section: {error}"),
            )
        })?;
        state.head = status.get("head").and_then(Value::as_u64).ok_or_else(|| {
            DriverError::coded(MALFORMED_STATUS, "`alder status --json` has no head")
        })?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_loop_section_is_read_and_the_rest_of_status_is_ignored() {
        let status = json!({
            "schema": "alder.status.v0",
            "head": 42,
            "ready": [{"id": "hm-9a1"}],
            "loop": {
                "paused": true,
                "pause_reason": "release freeze",
                "engine": "codex",
                "rotate_requested_seq": 17,
                "nudge_requested_seq": 41,
                "review_at": "2026-08-04T15:00:00Z",
                "review_deadlines": ["2026-08-04T15:00:00Z", "2026-08-05T09:00:00Z"]
            }
        });
        let state = LoopState::from_status(&status).unwrap();
        assert_eq!(state.head, 42);
        assert!(state.paused);
        assert_eq!(state.nudge_requested_seq, Some(41));
        assert_eq!(
            state
                .review_deadlines
                .iter()
                .map(|deadline| deadline.to_rfc3339())
                .collect::<Vec<_>>(),
            ["2026-08-04T15:00:00+00:00", "2026-08-05T09:00:00+00:00"]
        );

        let empty = LoopState::from_status(&json!({
            "head": 0,
            "loop": {"paused": false, "engine": null, "rotate_requested_seq": null,
                     "nudge_requested_seq": null, "review_at": null, "review_deadlines": []}
        }))
        .unwrap();
        assert!(!empty.paused);
        assert!(empty.nudge_requested_seq.is_none());
        assert!(empty.review_deadlines.is_empty());

        assert!(LoopState::from_status(&json!({"head": 1})).is_err());
        assert!(LoopState::from_status(&json!({"loop": 7})).is_err());
        // A status document with no head cannot answer the wake rule.
        assert!(LoopState::from_status(&json!({"loop": {"paused": false}})).is_err());
    }

    #[test]
    fn a_loop_section_without_paused_is_malformed_never_default_unpaused() {
        // The exact partial document from the review: a loop section with no
        // `paused`. Reading it as unpaused would run the command over a real
        // pause the truncation ate.
        let error = LoopState::from_status(&json!({"head": 42, "loop": {}}))
            .expect_err("a paused-less loop section must not decide anything");
        assert!(error.is(MALFORMED_STATUS), "{error}");

        // Every malformed shape carries the code, so the driver counts it
        // toward the outage notice rather than logging it invisibly forever.
        for document in [
            json!({"head": 1}),
            json!({"loop": 7}),
            json!({"loop": {"paused": false}}),
            json!({"head": "not a number", "loop": {"paused": false}}),
        ] {
            let error = LoopState::from_status(&document).expect_err("malformed");
            assert!(error.is(MALFORMED_STATUS), "{document}: {error}");
        }
    }
}
