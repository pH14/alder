use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{DriverError, Result};

/// The loop section of `alder status --json`, plus the head that document
/// reported. This is the driver's complete view of the durable log: it never
/// reads work, attempts, or questions, because deciding anything about them
/// would be judgment.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LoopState {
    /// The current log head. Compared with the last pass's `ended_seq`, this
    /// is the whole log trigger, and it needs no memory of its own.
    #[serde(default)]
    pub head: u64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub pause_reason: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub rotate_pending: bool,
    #[serde(default)]
    pub nudge_pending: bool,
    #[serde(default)]
    pub open_pass: Option<OpenPass>,
    #[serde(default)]
    pub last_pass: Option<LastPass>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenPass {
    pub id: String,
    pub engine: String,
    pub handle: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LastPass {
    pub id: String,
    pub outcome: Option<String>,
    pub wake_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    /// The head at which this pass ended.
    pub ended_seq: Option<u64>,
}

impl LoopState {
    /// Extract the loop section and the head, ignoring everything else
    /// `status` reports.
    pub fn from_status(status: &Value) -> Result<Self> {
        let section = status
            .get("loop")
            .ok_or_else(|| DriverError::new("`alder status --json` has no loop section"))?;
        let mut state: Self = serde_json::from_value(section.clone())
            .map_err(|error| DriverError::new(format!("unreadable loop section: {error}")))?;
        state.head = status
            .get("head")
            .and_then(Value::as_u64)
            .ok_or_else(|| DriverError::new("`alder status --json` has no head"))?;
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
                "rotate_pending": true,
                "nudge_pending": true,
                "open_pass": {
                    "id": "hm-pass-3",
                    "engine": "claude",
                    "handle": "tmux:alder-leader",
                    "triggers": ["log"],
                    "started_at": "2026-07-27T12:00:00Z",
                    "at_head": 41
                },
                "last_pass": {
                    "id": "hm-pass-2",
                    "engine": "claude",
                    "outcome": "ok",
                    "report_line": "swept the frontier",
                    "wake_at": "2026-07-27T12:20:00Z",
                    "ended_at": "2026-07-27T12:00:00Z",
                    "ended_seq": 38
                }
            }
        });
        let state = LoopState::from_status(&status).unwrap();
        assert_eq!(state.head, 42);
        assert!(state.paused);
        assert_eq!(state.pause_reason.as_deref(), Some("release freeze"));
        assert_eq!(state.engine.as_deref(), Some("codex"));
        assert!(state.rotate_pending);
        assert!(state.nudge_pending);
        assert_eq!(state.open_pass.as_ref().unwrap().id, "hm-pass-3");
        assert_eq!(state.open_pass.as_ref().unwrap().engine, "claude");
        assert_eq!(
            state.last_pass.as_ref().unwrap().outcome.as_deref(),
            Some("ok")
        );
        assert!(state.last_pass.as_ref().unwrap().wake_at.is_some());
        assert_eq!(state.last_pass.as_ref().unwrap().ended_seq, Some(38));

        let empty = LoopState::from_status(&json!({
            "head": 0,
            "loop": {"paused": false, "engine": null, "rotate_pending": false,
                     "nudge_pending": false, "open_pass": null, "last_pass": null}
        }))
        .unwrap();
        assert!(!empty.paused);
        assert!(empty.open_pass.is_none());

        assert!(LoopState::from_status(&json!({"head": 1})).is_err());
        assert!(LoopState::from_status(&json!({"loop": 7})).is_err());
        // A status document with no head cannot answer the log trigger.
        assert!(LoopState::from_status(&json!({"loop": {"paused": false}})).is_err());
    }
}
