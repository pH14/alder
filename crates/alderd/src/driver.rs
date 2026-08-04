//! The main loop. It reads the fold, decides with [`crate::decide`], and
//! performs effects through [`Effects`]. It never inspects work, attempts, or
//! questions, it never composes a prompt beyond the injection line, and it
//! appends nothing to the log — the log never mentions its own readers.
//!
//! What the driver has to remember — the last head it acted on, and when — is
//! machine-local [`Notes`] persisted under `.alder/`. Losing them is harmless:
//! the next poll delivers one wake more than it needed to, the leader reads
//! the fold, finds nothing new, and idles.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    decide::{
        self, Decision, EngineChoice, Notes, Notice, Poll, Session, SessionAction, Trigger, Wait,
        content_hash, injection, resolve_engine, rotate_pending, session_action,
    },
    effects::Effects,
    error::{DriverError, Result},
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
    /// daemon does not re-deliver a wake for state it already acted on — and
    /// tolerated when missing, because an extra wake is harmless.
    notes: Notes,
    /// What the daemon remembers about the session it launched. Forgotten on
    /// restart, which restarts the session rather than adopting a stranger.
    session: Option<Session>,
    /// When the current fire condition first held, for debouncing.
    pending_since: Option<DateTime<Utc>>,
    /// Whether the next injection must point the engine at the pass document.
    bootstrap: bool,
    /// Keeps a standing engine problem from pinging the operator every poll.
    engine_notice: Notice,
    outages: u32,
}

impl<E: Effects> Driver<E> {
    pub fn new(effects: E, config: Config) -> Self {
        let notes = Self::load_notes(&effects);
        Self {
            effects,
            config,
            notes,
            session: None,
            pending_since: None,
            bootstrap: false,
            engine_notice: Notice::default(),
            outages: 0,
        }
    }

    pub fn effects(&self) -> &E {
        &self.effects
    }

    /// Read the notes back, or start fresh. A missing or unreadable file is
    /// not an error: fresh notes mean one wake the driver did not strictly
    /// need to deliver, which is the harmless direction.
    fn load_notes(effects: &E) -> Notes {
        effects
            .read_file(Path::new(NOTES_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist the notes. A failed write is logged and tolerated: the worst
    /// outcome is a duplicate wake after a restart, never a lost fact.
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
    /// `baseline` cuts the wait short, so a Mac-side append is noticed in
    /// about `hintPollSeconds` rather than up to `pollSeconds`. A missing or
    /// stale marker changes nothing: the interval simply runs its course.
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

        if state.paused {
            self.pending_since = None;
            self.effects.log("idle: the loop is paused");
            return Ok(());
        }

        let refresh_changed = self.refresh_changed();
        // A changed sweep appended observation events, so the head just read
        // is already stale. Deciding on it would note a head one sweep behind
        // and deliver a second wake next poll for this driver's own appends,
        // so the status is read once more before deciding.
        let state = if refresh_changed {
            let state = self.observed_state()?;
            if state.paused {
                self.pending_since = None;
                self.effects.log("idle: the loop is paused");
                return Ok(());
            }
            state
        } else {
            state
        };

        let poll = Poll {
            now: self.effects.now(),
            refresh_changed,
            pending_since: self.pending_since,
            attached_client: self.attached_client(),
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

    /// The observation sweep. A failed refresh is not a trigger; it is simply
    /// no information, and the log and time triggers still apply.
    fn refresh_changed(&self) -> bool {
        match self.effects.alder(&["refresh"]) {
            Ok(document) => document
                .get("changed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            Err(error) => {
                self.effects.log(&format!("refresh failed: {error}"));
                false
            }
        }
    }

    fn attached_client(&self) -> bool {
        self.effects
            .tmux_has_clients(&self.config.tmux_session)
            .unwrap_or(false)
    }

    fn pass_doc_hash(&self) -> u64 {
        match self
            .effects
            .read_file(&PathBuf::from(&self.config.pass_doc))
        {
            Ok(bytes) => content_hash(&bytes),
            // An unreadable pass document must not silently look unchanged, so
            // hash its absence distinctly and let the engine report the error.
            Err(_) => content_hash(b""),
        }
    }

    fn fire(&mut self, state: &LoopState, triggers: &[Trigger]) -> Result<()> {
        let engine = match resolve_engine(&self.config, state) {
            EngineChoice::Run(engine) => {
                // The condition cleared; a later problem is news again.
                self.engine_notice.clear();
                engine
            }
            EngineChoice::Unknown(engine) => {
                return self
                    .engine_problem(&format!("engine `{engine}` is not configured on this host"));
            }
            EngineChoice::Ambiguous => {
                return self.engine_problem("no engine is selected; run `alder loop use <engine>`");
            }
        };
        let pass_doc_hash = self.pass_doc_hash();
        // The session is reconciled before the notes move on purpose: acting
        // is what consumes a pending rotation, so rotating first means a crash
        // between the restart and the notes write merely re-rotates next fire,
        // while the reverse order would consume a rotation without performing
        // it. The cost — an occasional redundant restart — is bounded.
        self.reconcile_session(state, &engine, pass_doc_hash)?;

        let message = injection(self.bootstrap, &self.config.pass_doc, triggers);
        self.effects
            .tmux_send_keys(&self.config.tmux_session, &message)?;
        self.bootstrap = false;
        self.effects.log(&format!("woke the leader: {message}"));

        // The delivery happened; note it, durably enough for a restart. The
        // order is deliberate: a crash before this write leaves stale notes,
        // and the next poll delivers the same wake again — harmless, because
        // the leader reads the fold and nothing durable records wakes.
        self.notes = Notes {
            last_head: state.head,
            last_wake_at: Some(self.effects.now()),
        };
        self.save_notes();
        Ok(())
    }

    /// A standing engine problem is reported once, not once per poll.
    fn engine_problem(&mut self, message: &str) -> Result<()> {
        if self.engine_notice.raise(message) {
            self.effects.notify(message);
        } else {
            self.effects.log(message);
        }
        Ok(())
    }

    fn reconcile_session(
        &mut self,
        state: &LoopState,
        engine: &str,
        pass_doc_hash: u64,
    ) -> Result<()> {
        let exists = self
            .effects
            .tmux_session_exists(&self.config.tmux_session)?;
        let now = self.effects.now();
        let action = session_action(
            &self.config,
            rotate_pending(state, &self.notes),
            engine,
            pass_doc_hash,
            exists,
            self.session.as_ref(),
            now,
        );
        match action {
            SessionAction::Reuse => return Ok(()),
            SessionAction::Restart(reason) => {
                self.effects.log(&format!("rotating the session: {reason}"));
                self.effects.tmux_kill_session(&self.config.tmux_session)?;
            }
            SessionAction::Create => {}
        }
        let configured = self
            .config
            .engines
            .get(engine)
            .ok_or_else(|| DriverError::new(format!("engine `{engine}` is not configured")))?;
        self.effects
            .tmux_new_session(&self.config.tmux_session, configured)?;
        self.session = Some(Session {
            engine: engine.to_owned(),
            pass_doc_hash,
            created_at: now,
        });
        // A fresh engine has read nothing, so the next injection must say
        // where the pass document lives.
        self.bootstrap = true;
        Ok(())
    }
}
