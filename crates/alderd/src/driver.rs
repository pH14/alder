//! The main loop. It reads four things, decides with [`crate::decide`], and
//! performs effects through [`Effects`]. It never inspects work, attempts, or
//! questions, and it never composes a prompt beyond the injection line.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    decide::{
        self, Decision, EngineChoice, Notice, Poll, Session, SessionAction, Trigger, content_hash,
        injection, observable_session, pass_timed_out, resolve_engine, session_action,
    },
    effects::Effects,
    error::{DriverError, Result},
    loop_state::LoopState,
};

/// Consecutive `store_unavailable` results before the driver says so out loud.
const OUTAGE_NOTICE_AFTER: u32 = 3;

pub struct Driver<E: Effects> {
    effects: E,
    config: Config,
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
        Self {
            effects,
            config,
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

    /// Run until the process is stopped.
    pub fn run(&mut self) -> ! {
        loop {
            if let Err(error) = self.poll_once() {
                self.effects.log(&format!("poll failed: {error}"));
            }
            self.effects.sleep(self.config.poll());
        }
    }

    /// One complete poll. Public so a caller can drive it step by step.
    pub fn poll_once(&mut self) -> Result<()> {
        let state = match self.loop_state() {
            Ok(state) => {
                self.outages = 0;
                state
            }
            Err(error) if error.is("store_unavailable") => {
                self.outages = self.outages.saturating_add(1);
                if self.outages == OUTAGE_NOTICE_AFTER {
                    self.effects.notify(&format!(
                        "the Alder store has been unavailable for {OUTAGE_NOTICE_AFTER} polls"
                    ));
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };

        // A pass that is already open owns the loop. Resolving it is the only
        // thing worth doing, and it is also how a daemon restart recovers.
        if state.open_pass.is_some() {
            return self.await_open_pass(&state);
        }
        if state.paused {
            self.pending_since = None;
            return Ok(());
        }

        let poll = Poll {
            now: self.effects.now(),
            refresh_changed: self.refresh_changed(),
            pending_since: self.pending_since,
            attached_client: self.attached_client(),
        };

        match decide::decide(&self.config, &state, &poll) {
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
        // The session is reconciled before the wake on purpose: the wake is
        // what consumes a pending rotation (by log order), so rotating first
        // means a crash between the two merely re-rotates next fire, while the
        // reverse order would consume a rotation without performing it. The
        // cost — a lost wake race can churn a session — is bounded and rare.
        self.reconcile_session(state, &engine, pass_doc_hash)?;

        // Intent before effects: the wake is durable before anything is typed
        // into the leader's terminal.
        let Some(pass_id) = self.wake(&engine, triggers)? else {
            return Ok(());
        };
        if let Some(session) = self.session.as_mut() {
            session.passes = session.passes.saturating_add(1);
        }

        let message = injection(self.bootstrap, &self.config.pass_doc, &pass_id, triggers);
        self.bootstrap = false;
        self.effects
            .tmux_send_keys(&self.config.tmux_session, &message)?;
        self.effects.log(&format!("woke {pass_id}: {message}"));
        let handle = format!("tmux:{}", self.config.tmux_session);
        self.await_pass(&pass_id, self.effects.now(), &handle)
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
        let action = session_action(
            &self.config,
            state,
            engine,
            pass_doc_hash,
            exists,
            self.session.as_ref(),
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
            passes: 0,
        });
        // A fresh engine has read nothing, so the next injection must say
        // where the pass document lives.
        self.bootstrap = true;
        Ok(())
    }

    /// Wake the loop, or concede.
    ///
    /// This poll's status read showed no open pass, so a `pass_open` conflict
    /// can only mean another writer — a second driver, or a human at a
    /// terminal — opened one in the last few seconds. That pass is almost
    /// certainly alive, and it is not this driver's to end. Conceding is the
    /// whole exclusion mechanism the loop has: the next poll sees the open
    /// pass, adopts it, and applies the stale rule with real timeout facts.
    ///
    /// A pass genuinely left over from a crash is never seen here, because the
    /// poll would have found it open and never reached the fire path.
    fn wake(&self, engine: &str, triggers: &[Trigger]) -> Result<Option<String>> {
        let handle = format!("tmux:{}", self.config.tmux_session);
        let mut args = vec!["loop", "wake", "--engine", engine, "--handle", &handle];
        for trigger in triggers {
            args.push("--trigger");
            args.push(trigger.as_str());
        }
        match self.effects.alder(&args) {
            Ok(document) => document
                .get("pass_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| Some(id.to_owned()))
                .ok_or_else(|| DriverError::new("`alder loop wake` reported no pass ID")),
            Err(error) if error.is("pass_open") => {
                self.effects
                    .log("another writer opened a pass first; conceding this wake");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Resume a pass this daemon did not start, or did not finish watching.
    ///
    /// This is where every leftover pass is resolved, whoever opened it: a
    /// daemon restart, a crashed engine, or another writer's pass that won a
    /// wake race. Nothing else in the driver ends a pass it did not open.
    fn await_open_pass(&mut self, state: &LoopState) -> Result<()> {
        let open = state.open_pass.as_ref().expect("checked by the caller");
        self.effects
            .log(&format!("awaiting the open pass {}", open.id));
        let (id, started_at, handle) = (open.id.clone(), open.started_at, open.handle.clone());
        self.await_pass(&id, started_at, &handle)
    }

    /// Poll until the pass ends, its session dies, or the ceiling is reached.
    ///
    /// `handle` is the pass's own recorded handle, not this driver's session
    /// name. A pass opened by another writer may name a different tmux session
    /// or no tmux session at all, and calling such a pass `crashed` because
    /// *this* driver's session is gone would be a lie.
    fn await_pass(&mut self, pass_id: &str, started_at: DateTime<Utc>, handle: &str) -> Result<()> {
        let own_session = handle == format!("tmux:{}", self.config.tmux_session);
        loop {
            let document = self.effects.alder(&["show", pass_id])?;
            let ended = document
                .pointer("/current/state")
                .and_then(serde_json::Value::as_str)
                == Some("ended");
            if ended {
                let outcome = document
                    .pointer("/current/outcome")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("ended")
                    .to_owned();
                self.effects.log(&format!("{pass_id} ended {outcome}"));
                if outcome != "ok" {
                    self.effects
                        .notify(&format!("pass {pass_id} ended {outcome}"));
                }
                return Ok(());
            }
            // `crashed` is a claim about an observed dead session, so it is
            // available only for a tmux handle the driver can actually check.
            if let Some(session) = observable_session(handle)
                && !self.effects.tmux_session_exists(session)?
            {
                self.end_pass(pass_id, "crashed", "the tmux session is gone")?;
                if own_session {
                    self.session = None;
                }
                self.effects.notify(&format!(
                    "pass {pass_id} crashed: the engine session is gone"
                ));
                return Ok(());
            }
            // Time is the only fact available for a handle the driver cannot
            // observe, so an unobservable pass can only ever time out.
            if pass_timed_out(&self.config, started_at, self.effects.now()) {
                self.end_pass(pass_id, "timeout", "the pass exceeded its time budget")?;
                self.effects.notify(&format!("pass {pass_id} timed out"));
                return Ok(());
            }
            self.effects.sleep(self.config.poll());
        }
    }

    fn end_pass(&self, pass_id: &str, outcome: &str, why: &str) -> Result<()> {
        self.effects
            .alder(&["pass", "end", pass_id, "--outcome", outcome, "--why", why])?;
        Ok(())
    }
}
