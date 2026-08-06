//! Check, message, or stop a run by its handle.
//!
//! A handle is the tmux session name printed by `start`. These operations read
//! the tier, worktree, and model state from the environment that the runner
//! recorded in that session. They do not look up the run anywhere else.

use std::path::Path;

use crate::{
    error::{EXIT_UNRECEIVABLE, Result, RunnerError},
    host::{EngineMarker, Host},
    start::{ENGINE_ENV, ENGINE_RUNNING, TORN_ENV},
    tier::Provider,
};

/// The largest file `send` will deliver. A Codex send puts the base64-encoded
/// text in tmux's argument list, which has a fixed size limit. The runner
/// refuses larger files instead of risking truncation.
pub const MAX_SEND_BYTES: u64 = 64 * 1024;

/// What `status <handle>` prints: one word, plus an optional detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `running`, `done`, or `dead`.
    pub word: &'static str,
    pub detail: Option<String>,
}

/// One word about one handle.
///
/// `dead` means no session answers to the handle. For tmux, `done` means the
/// model process exited and its holding shell remains. It says nothing about
/// whether the run succeeded; the branch contains the result. A session with
/// no model-state marker is reported as `running` because the runner cannot
/// safely assume that an unfamiliar session is finished.
pub fn status(host: &Host, handle: &str) -> Result<Status> {
    use crate::host::RunnerHost;
    let Some(observed) = host.tmux_session(handle)? else {
        return Ok(Status {
            word: "dead",
            detail: None,
        });
    };
    // Only a proven-exited engine reads `done`; a session that cannot prove
    // its engine's state reads `running`, never presumed finished.
    let word = if observed.engine == EngineMarker::Exited {
        "done"
    } else {
        "running"
    };
    let detail = match (&observed.tier, &observed.worktree) {
        (Some(tier), Some(worktree)) => {
            Some(format!("tier {tier}, worktree {}", worktree.display()))
        }
        (Some(tier), None) => Some(format!("tier {tier}")),
        (None, Some(worktree)) => Some(format!("worktree {}", worktree.display())),
        (None, None) => None,
    };
    Ok(Status { word, detail })
}

/// What `kill <handle>` did, verified rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Killed {
    /// The session existed, was killed, and is verified gone.
    Killed,
    /// Nothing answered to the handle in the first place. Deliberately not an
    /// error — the caller kills to be sure, not because it knows — but a
    /// distinct message, so "I ended it" and "there was nothing" read apart.
    AlreadyDead,
}

/// Stop one run while holding its lock, then verify that it stopped.
///
/// `kill` reports success only after checking that no session answers to the
/// handle. If the session remains, the tmux exit status explains the failure.
/// If the tmux command fails but another process has already removed the
/// session, the final check still reports success.
pub fn kill(host: &Host, handle: &str) -> Result<Killed> {
    use crate::host::RunnerHost;
    let _lock = host.lock_handle(handle)?;
    if host.tmux_session(handle)?.is_none() {
        return Ok(Killed::AlreadyDead);
    }
    let killed = host.tmux_kill_session(handle);
    if host.tmux_session(handle)?.is_some() {
        return Err(match killed {
            Err(error) => error,
            Ok(()) => RunnerError::new(format!(
                "tmux reported killing `{handle}`, but the session still exists"
            )),
        });
    }
    Ok(Killed::Killed)
}

/// Deliver a local file's contents as input to the run.
///
/// For an interactive Claude run, the file is loaded into a tmux buffer and
/// pasted as text, so its bytes cannot become shell syntax or tmux key names.
/// For a one-shot Codex run, the text is base64-encoded into a command that
/// uses the generated resume script to resume the recorded session. The
/// runner does not inspect the pane or wait for the model to acknowledge the
/// message. It reports one accepted send after submitting the message.
///
/// A send can stop between pasting the text and pressing Enter, leaving
/// unsubmitted text in the pane. If Enter fails, `send` retries it once. If
/// the retry fails, it records a torn-send marker and reports the incomplete
/// send. Later sends refuse the pane because another paste could mix two
/// messages. A person can stop the pane or submit its text, or pass `force`
/// to send anyway. A forced send clears the marker after its own Enter
/// succeeds.
///
/// `start` records the selected provider in the session. `send` uses that
/// value instead of the current tier table, so a later configuration change
/// cannot change how a live session receives messages.
///
/// Exit code 0 means one accepted delivery. Code 4 means
/// another operation holds the handle's lock — the caller should treat the
/// message as already served by the lock winner, never kill the session over
/// it. Code 5 means the run cannot receive this delivery — nothing answers to
/// the handle, the model exited, an earlier send was incomplete, or the Codex
/// session cannot be resumed — and the caller may replace the run. Every
/// other failure uses code 1.
pub fn send(host: &Host, handle: &str, file: &Path, force: bool) -> Result<()> {
    use crate::host::RunnerHost;
    if !file.is_file() {
        return Err(RunnerError::new(format!(
            "cannot read local file `{}`",
            file.display()
        )));
    }
    let size = file
        .metadata()
        .map_err(|error| RunnerError::new(format!("cannot inspect `{}`: {error}", file.display())))?
        .len();
    if size > MAX_SEND_BYTES {
        return Err(RunnerError::new(format!(
            "`{}` is {size} bytes; send refuses files larger than 64 KiB \
             (the armored delivery route rides tmux argv, which has a hard \
             ceiling). Deliver a pointer to the file instead",
            file.display()
        )));
    }
    // The same per-handle lock `start` and `kill` take: two concurrent sends
    // must not interleave their paste and Enter into one pane. The loser
    // refuses, for symmetry with `start`, rather than queueing a message the
    // caller believes was delivered promptly.
    let _lock = host.lock_handle(handle)?;
    let observed = host.tmux_session(handle)?.ok_or_else(|| {
        RunnerError::refusal(
            EXIT_UNRECEIVABLE,
            format!("no execution answers to `{handle}`"),
        )
    })?;
    let provider = observed.provider.as_deref().ok_or_else(|| {
        RunnerError::new(format!(
            "session `{handle}` carries no provider stamp; it is not this runner's"
        ))
    })?;
    let provider = Provider::parse(provider).map_err(|error| {
        RunnerError::new(format!(
            "session `{handle}` carries an unusable provider stamp: {error}"
        ))
    })?;

    // A previous send tore between paste and Enter: the pane holds
    // unsubmitted text, and pasting more at it would mix two messages.
    let torn = host.tmux_environment(handle, TORN_ENV)?.is_some();
    if torn && !force {
        return Err(RunnerError::refusal(
            EXIT_UNRECEIVABLE,
            format!(
                "the pane for `{handle}` holds unsubmitted text from a torn send; \
             kill or submit it first, or pass --force to deliver anyway"
            ),
        ));
    }

    let buffer = format!("alder-ext-send-{}", std::process::id());
    let delivered = match provider {
        Provider::Claude => {
            if observed.engine == EngineMarker::Exited {
                return Err(RunnerError::refusal(
                    EXIT_UNRECEIVABLE,
                    format!(
                        "cannot deliver to the exited interactive engine for `{handle}`; \
                     start a fresh execution"
                    ),
                ));
            }
            if observed.engine != EngineMarker::Running {
                // Fail-safe: never paste at a pane that cannot prove an
                // engine is running to receive it.
                return Err(RunnerError::refusal(
                    EXIT_UNRECEIVABLE,
                    format!(
                        "session `{handle}` cannot prove an engine is running (no engine \
                     marker); refusing to paste at it"
                    ),
                ));
            }
            // tmux reads the local file itself into a server buffer; no
            // command substitution can trim a newline or make its contents
            // shell syntax.
            host.tmux_load_buffer(&buffer, file)
                .and_then(|()| host.tmux_paste_buffer(&buffer, handle))
        }
        Provider::Codex => {
            // Codex delivery is always encoded, including while its one-shot
            // engine is still running, so the text somebody else wrote never
            // becomes shell syntax. The command sits in the pane's input until
            // a holding shell can run it. Both the session marker and the
            // resume script live in the runner-owned state directory: the
            // runner never trusts or executes worktree contents, which the
            // execution itself writes.
            let state = host.handle_state_dir(handle);
            let marker = state.join("codex-session");
            let codex_session = std::fs::read_to_string(&marker)
                .map_err(|error| {
                    RunnerError::refusal(
                        EXIT_UNRECEIVABLE,
                        format!(
                            "cannot resume `{handle}`: no codex session recorded at `{}`: {error}",
                            marker.display()
                        ),
                    )
                })?
                .trim()
                .to_owned();
            // The producer's contract (the stamp sidecar in tier.rs) is a
            // lowercase UUID; anything else — including an option-like string
            // — is not a session ID and must never reach a command line.
            if !is_codex_session_id(&codex_session) {
                return Err(RunnerError::refusal(
                    EXIT_UNRECEIVABLE,
                    format!(
                        "the recorded codex session for `{handle}` is not a session ID \
                     (expected a lowercase UUID)"
                    ),
                ));
            }
            let bytes = std::fs::read(file).map_err(|error| {
                RunnerError::new(format!("cannot read `{}`: {error}", file.display()))
            })?;
            let encoded = base64(&bytes);
            let resume = crate::host::quote(&state.join("resume").display().to_string());
            // `base64 -d` on Linux, `-D` on the BSD userland; whichever the
            // box has decodes the same armored bytes.
            let command = format!(
                "message=$(printf %s {encoded} | base64 -d 2>/dev/null || \
                 printf %s {encoded} | base64 -D); \
                 {resume} {codex_session} \"$message\""
            );
            host.tmux_set_buffer(&buffer, &command)
                .and_then(|()| host.tmux_paste_buffer(&buffer, handle))
        }
    };
    if let Err(error) = delivered {
        host.tmux_delete_buffer(&buffer);
        return Err(error);
    }
    // The interactive route re-checks the engine marker between paste and
    // Enter. The pane sets its exited marker before `exec bash`, so an engine
    // that died since the first check is visible here — and the pasted bytes
    // are then sitting at a shell, where Enter would EXECUTE them. Back the
    // text out (C-u) and refuse loudly. A residual window remains between
    // this re-check and the Enter below; it is documented in the README and
    // accepted: closing it entirely would require the pane to prove receipt,
    // which no engine offers.
    if provider == Provider::Claude
        && host.tmux_environment(handle, ENGINE_ENV)?.as_deref() != Some(ENGINE_RUNNING)
    {
        if let Err(clear) = host.tmux_discard_input(handle) {
            return Err(RunnerError::refusal(
                EXIT_UNRECEIVABLE,
                format!(
                    "the engine for `{handle}` exited between paste and submit, and \
                 the pasted text could NOT be cleared ({clear}); do not press \
                 Enter in that pane — clear its input line by hand"
                ),
            ));
        }
        return Err(RunnerError::refusal(
            EXIT_UNRECEIVABLE,
            format!(
                "the engine for `{handle}` exited between paste and submit; the \
             pasted text was cleared (C-u) and nothing was submitted. Start a \
             fresh execution instead"
            ),
        ));
    }
    // The paste landed; from here the only honest outcomes are "submitted"
    // or "torn, and the session says so". One immediate retry covers a
    // transiently busy server; a second failure is recorded on the session
    // itself so every later send refuses the dirty pane.
    if let Err(first) = host.tmux_submit(handle)
        && let Err(second) = host.tmux_submit(handle)
    {
        if let Err(mark) = host.tmux_set_session_environment(handle, TORN_ENV, "1") {
            eprintln!(
                "alder-ext-runner: could not stamp the torn-send marker on \
                 `{handle}`: {mark}"
            );
        }
        return Err(RunnerError::refusal(
            EXIT_UNRECEIVABLE,
            format!(
                "DELIVERY TORN for `{handle}`: the text was pasted but Enter failed \
             twice ({first}; retry: {second}). Unsubmitted text sits in the pane; \
             kill or submit it by hand, or resend with --force"
            ),
        ));
    }
    if torn {
        // `--force` delivered over a torn pane and its Enter landed, which
        // submits the residue along with this message: resolved.
        if let Err(clear) = host.tmux_unset_session_environment(handle, TORN_ENV) {
            eprintln!(
                "alder-ext-runner: could not clear the torn-send marker on \
                 `{handle}`: {clear}"
            );
        }
    }
    println!("sent once to {handle}");
    Ok(())
}

/// Return true only for the lowercase UUID format written by the Codex
/// session-ID helper. Options, uppercase hexadecimal digits, and other
/// near-matches are rejected.
fn is_codex_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
    })
}

/// Standard base64, no line breaks: what `base64 -d` reads back.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        encoded.push(ALPHABET[(word >> 18) as usize & 63] as char);
        encoded.push(ALPHABET[(word >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(word >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[word as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 test vectors: what `base64 -d` will decode.
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input), expected);
        }
        // Newlines and shell syntax armor cleanly.
        assert_eq!(base64(b"a\nb'c\"d$(x)"), "YQpiJ2MiZCQoeCk=");
    }

    #[test]
    fn only_a_lowercase_uuid_is_a_codex_session_id() {
        assert!(is_codex_session_id("019fb2ef-d507-7201-bc36-79d6d5b82336"));

        for wrong in [
            "",
            "--last",
            "-r",
            "$(rm -rf /)",
            // Uppercase hex: valid to a UUID parser, not to the producer.
            "019FB2EF-D507-7201-BC36-79D6D5B82336",
            // Right alphabet, wrong shape.
            "019fb2efd5077201bc3679d6d5b82336",
            "019fb2ef-d507-7201-bc36-79d6d5b8233",
            "019fb2ef-d507-7201-bc36-79d6d5b823367",
            "019fb2ef-d507-7201-bc36-79d6d5b8233g",
            "019fb2ef_d507_7201_bc36_79d6d5b82336",
        ] {
            assert!(!is_codex_session_id(wrong), "{wrong} was accepted");
        }
    }
}
