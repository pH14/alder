//! The handle verbs: `status`, `send`, `kill`.
//!
//! A handle is the tmux session name `start` printed, and everything these
//! verbs know about the execution they name is read back from the session's
//! own environment — the tier, the worktree, and whether the engine is still
//! running. Nothing here consults any other system.

use std::path::Path;

use crate::{
    error::{Result, RunnerError},
    host::{EngineMarker, Host},
    start::{RUNNER_DIR, TORN_ENV},
    tier::{Provider, Tier},
};

/// What `status <handle>` prints: one word, plus an optional detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `running`, `done`, or `dead`.
    pub word: &'static str,
    pub detail: Option<String>,
}

/// One word about one handle.
///
/// `dead` means no session answers to the handle. `done` is venue-specific
/// best effort: for tmux it means the engine process exited and its holding
/// shell remains — it says nothing about whether the run succeeded, only that
/// nothing is still running. The result, either way, is whatever the branch
/// holds. A session that exists but carries no engine marker is reported
/// `running`, because a session of unknown provenance must never be presumed
/// finished.
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

/// End one execution. Killing what is not there is deliberately not an
/// error: the caller kills to be sure, not because it knows.
pub fn kill(host: &Host, handle: &str) -> Result<()> {
    use crate::host::RunnerHost;
    host.tmux_kill_session(handle)
}

/// Deliver a local file's contents as input to the running execution.
///
/// This is the old relay craft, internal to the runner: for an interactive
/// engine (claude) the file is loaded into a tmux buffer and pasted, so no
/// byte of it can become shell syntax or a key name; for a one-shot engine
/// (codex) the bytes are base64-armored into a command that resumes the
/// recorded codex session through the generated resume script, since a
/// one-shot engine has no prompt to paste at. In neither route does the
/// runner inspect the pane or synchronize on the execution's progress:
/// delivery is at-least-once and reports exactly one accepted send.
///
/// **The torn-send contract.** A delivery has two effects — paste, then one
/// submitting Enter — and can tear between them, leaving pasted text sitting
/// unsubmitted in the pane. When Enter fails, `send` retries it once
/// immediately; if that also fails it stamps the session with a torn marker
/// and reports loudly that unsubmitted text sits in the pane. Every later
/// `send` sees the marker and refuses — the pane is dirty, and pasting more
/// text at it would corrupt whatever a human or the engine makes of the
/// residue — until someone resolves it: kill or submit the pane by hand, or
/// pass `force`, which delivers anyway and clears the marker once its own
/// Enter lands.
pub fn send(
    host: &Host,
    table: &'static [Tier],
    handle: &str,
    file: &Path,
    force: bool,
) -> Result<()> {
    use crate::host::RunnerHost;
    if !file.is_file() {
        return Err(RunnerError::new(format!(
            "cannot read local file `{}`",
            file.display()
        )));
    }
    let observed = host
        .tmux_session(handle)?
        .ok_or_else(|| RunnerError::new(format!("no execution answers to `{handle}`")))?;
    let tier_name = observed.tier.as_deref().ok_or_else(|| {
        RunnerError::new(format!(
            "session `{handle}` carries no tier marker; it is not this runner's"
        ))
    })?;
    let tier = crate::tier::lookup(table, tier_name)?;

    // A previous send tore between paste and Enter: the pane holds
    // unsubmitted text, and pasting more at it would mix two messages.
    let torn = host.tmux_environment(handle, TORN_ENV)?.is_some();
    if torn && !force {
        return Err(RunnerError::new(format!(
            "the pane for `{handle}` holds unsubmitted text from a torn send; \
             kill or submit it first, or pass --force to deliver anyway"
        )));
    }

    let buffer = format!("alder-ext-send-{}", std::process::id());
    let delivered = match tier.provider {
        Provider::Claude => {
            if observed.engine == EngineMarker::Exited {
                return Err(RunnerError::new(format!(
                    "cannot deliver to the exited interactive engine for `{handle}`; \
                     start a fresh execution"
                )));
            }
            if observed.engine != EngineMarker::Running {
                // Fail-safe: never paste at a pane that cannot prove an
                // engine is running to receive it.
                return Err(RunnerError::new(format!(
                    "session `{handle}` cannot prove an engine is running (no engine \
                     marker); refusing to paste at it"
                )));
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
            // a holding shell can run it.
            let worktree = observed.worktree.as_deref().ok_or_else(|| {
                RunnerError::new(format!(
                    "session `{handle}` carries no worktree marker; cannot find its \
                     codex session"
                ))
            })?;
            let marker = worktree.join(RUNNER_DIR).join("codex-session");
            let codex_session = std::fs::read_to_string(&marker)
                .map_err(|error| {
                    RunnerError::new(format!(
                        "cannot resume `{handle}`: no codex session recorded at `{}`: {error}",
                        marker.display()
                    ))
                })?
                .trim()
                .to_owned();
            if codex_session.is_empty()
                || !codex_session
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
            {
                return Err(RunnerError::new(format!(
                    "the recorded codex session for `{handle}` is not a session ID"
                )));
            }
            let bytes = std::fs::read(file).map_err(|error| {
                RunnerError::new(format!("cannot read `{}`: {error}", file.display()))
            })?;
            let encoded = base64(&bytes);
            // `base64 -d` on Linux, `-D` on the BSD userland; whichever the
            // box has decodes the same armored bytes.
            let command = format!(
                "message=$(printf %s {encoded} | base64 -d 2>/dev/null || \
                 printf %s {encoded} | base64 -D); \
                 {RUNNER_DIR}/resume {codex_session} \"$message\""
            );
            host.tmux_set_buffer(&buffer, &command)
                .and_then(|()| host.tmux_paste_buffer(&buffer, handle))
        }
    };
    if let Err(error) = delivered {
        host.tmux_delete_buffer(&buffer);
        return Err(error);
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
        return Err(RunnerError::new(format!(
            "DELIVERY TORN for `{handle}`: the text was pasted but Enter failed \
             twice ({first}; retry: {second}). Unsubmitted text sits in the pane; \
             kill or submit it by hand, or resend with --force"
        )));
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
}
