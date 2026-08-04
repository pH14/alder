//! The main loop. It reads the fold, decides with [`crate::decide`], and
//! performs effects through [`Effects`]. It never inspects work, attempts, or
//! questions, it composes no prompt, and it appends nothing to the log — the
//! log never mentions its own readers. When a trigger fires it runs the
//! configured command, with the trigger names in `ALDERD_TRIGGERS`; sessions
//! and engines are that command's business, never this daemon's.
//!
//! What the driver has to remember — the last head it acted on, and when — is
//! machine-local [`Notes`] persisted under `.alder/`. Losing them is harmless:
//! the next poll runs the command once more than it needed to, the executor
//! behind it reads the fold, finds nothing new, and idles.

use std::{path::Path, time::Duration};

use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    decide::{self, Decision, Notes, Poll, Trigger, Wait, trigger_names},
    effects::Effects,
    error::Result,
    loop_state::LoopState,
};

/// Consecutive `store_unavailable` results before the driver says so out loud.
const OUTAGE_NOTICE_AFTER: u32 = 3;

/// The local append marker the Alder CLI touches after each confirmed append.
/// Statting it is not a read of the log; it only shortens the driver's sleep.
const APPEND_MARKER: &str = ".alder/last-append";

/// Where this driver keeps its notes: the last head it acted on and when.
/// Machine-local and gitignored, like everything else under `.alder/`.
const NOTES_FILE: &str = ".alder/alderd-notes.json";

pub struct Driver<E: Effects> {
    effects: E,
    config: Config,
    /// The last head this driver acted on, and when. Persisted so a restarted
    /// daemon does not re-run the command for state it already acted on — and
    /// tolerated when missing, because an extra run is harmless.
    notes: Notes,
    /// When the current fire condition first held, for debouncing.
    pending_since: Option<DateTime<Utc>>,
    outages: u32,
}

impl<E: Effects> Driver<E> {
    pub fn new(effects: E, config: Config) -> Self {
        let notes = Self::load_notes(&effects);
        Self {
            effects,
            config,
            notes,
            pending_since: None,
            outages: 0,
        }
    }

    pub fn effects(&self) -> &E {
        &self.effects
    }

    /// Read the notes back, or start fresh. A missing or unreadable file is
    /// not an error: fresh notes mean one run the driver did not strictly
    /// need to deliver, which is the harmless direction.
    fn load_notes(effects: &E) -> Notes {
        effects
            .read_file(Path::new(NOTES_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist the notes. A failed write is logged and tolerated: the worst
    /// outcome is a duplicate run after a restart, never a lost fact.
    fn save_notes(&self) {
        let bytes = serde_json::to_vec_pretty(&self.notes).expect("notes serialize");
        if let Err(error) = self.effects.write_file(Path::new(NOTES_FILE), &bytes) {
            self.effects
                .log(&format!("cannot persist the driver notes: {error}"));
        }
    }

    /// Run until the process is stopped.
    pub fn run(&mut self) -> ! {
        loop {
            let baseline = self.effects.now();
            if let Err(error) = self.poll_once() {
                self.effects.log(&format!("poll failed: {error}"));
            }
            self.sleep_between_polls(baseline);
        }
    }

    /// Wait out one poll interval in hint-sized slices, statting the local
    /// append marker between slices. An append on this machine after
    /// `baseline` cuts the wait short, so a local append is noticed in about
    /// `hintPollSeconds` rather than up to `pollSeconds`. A missing or stale
    /// marker changes nothing: the interval simply runs its course.
    fn sleep_between_polls(&self, baseline: DateTime<Utc>) {
        let mut waited = Duration::ZERO;
        loop {
            let marker = self.effects.file_mtime(Path::new(APPEND_MARKER));
            match decide::next_wait(&self.config, waited, baseline, marker) {
                Wait::Poll(reason) => {
                    if waited < self.config.poll() {
                        self.effects.log(&format!("polling early: {reason}"));
                    }
                    return;
                }
                Wait::Sleep(step) => {
                    self.effects.sleep(step);
                    waited += step;
                }
            }
        }
    }

    /// One complete poll. Public so a caller can drive it step by step.
    pub fn poll_once(&mut self) -> Result<()> {
        let state = self.observed_state()?;

        let poll = Poll {
            now: self.effects.now(),
            pending_since: self.pending_since,
        };

        match decide::decide(&self.config, &state, &self.notes, &poll) {
            Decision::Idle(reason) => {
                self.pending_since = None;
                self.effects.log(&format!("idle: {reason}"));
                Ok(())
            }
            Decision::Hold(reason) => {
                self.pending_since.get_or_insert(poll.now);
                self.effects.log(&format!("holding: {reason}"));
                Ok(())
            }
            Decision::Fire(triggers) => {
                self.pending_since = None;
                self.fire(&state, &triggers)
            }
        }
    }

    fn loop_state(&self) -> Result<LoopState> {
        LoopState::from_status(&self.effects.alder(&["status"])?)
    }

    /// Read the loop state, counting consecutive store outages so a standing
    /// outage is announced once rather than every poll.
    fn observed_state(&mut self) -> Result<LoopState> {
        match self.loop_state() {
            Ok(state) => {
                self.outages = 0;
                Ok(state)
            }
            Err(error) if error.is("store_unavailable") => {
                self.outages = self.outages.saturating_add(1);
                if self.outages == OUTAGE_NOTICE_AFTER {
                    self.effects.notify(&format!(
                        "the Alder store has been unavailable for {OUTAGE_NOTICE_AFTER} polls"
                    ));
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Run the configured command, then note the head it ran for.
    ///
    /// The order is deliberate: command success before the notes write, so a
    /// crash — or a command failure — leaves stale notes and the next poll
    /// runs the same wake again. That is the harmless direction, because the
    /// command is idempotent by design: whatever it drives reads the fold,
    /// and nothing durable records wakes.
    fn fire(&mut self, state: &LoopState, triggers: &[Trigger]) -> Result<()> {
        let names = trigger_names(triggers);
        self.effects.run_command(&self.config.command, &names)?;
        self.effects
            .log(&format!("ran the command (triggers: {names})"));

        self.notes = Notes {
            last_head: state.head,
            last_wake_at: Some(self.effects.now()),
        };
        self.save_notes();
        Ok(())
    }
}
