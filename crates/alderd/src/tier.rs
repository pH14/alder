//! The dispatch ladder: six named rungs, each pinning a model and an effort.
//!
//! A tier name is the only thing a dispatcher ever types. Everything the
//! engine is actually run with — provider, model, reasoning effort, sandbox
//! and approval policy — is pinned here, in one table, so that "which model
//! did this attempt run on" is answered by the log rather than by whatever the
//! CLI's own default happened to be that week.
//!
//! That is the whole reason an unknown tier is a hard error. Falling through
//! to a CLI default would launch a worker at an unknown model and an unknown
//! effort, record nothing about it, and look exactly like a successful
//! dispatch.

use crate::error::{DriverError, Result};

/// Which CLI runs a rung, and which account its spend and rate limits land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
        }
    }

    /// Both providers, in ladder order. Used by `alderd budget` so it reports
    /// every provider whether or not anything was spent on it.
    pub const ALL: [Provider; 2] = [Provider::Codex, Provider::Claude];

    pub fn parse(name: &str) -> Result<Self> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.as_str() == name)
            .ok_or_else(|| {
                DriverError::new(format!(
                    "unknown provider `{name}`; known providers are codex and claude"
                ))
            })
    }
}

/// One rung of the ladder.
#[derive(Debug)]
pub struct Tier {
    pub name: &'static str,
    pub provider: Provider,
    /// The full model name, as it is passed to the CLI and stamped on the
    /// attempt. Never an alias: an alias moves under us.
    pub model: &'static str,
    pub effort: &'static str,
    /// The rung of equal standing on the other provider's ladder, used when
    /// this one's provider is rate-limited.
    pub counterpart: &'static str,
}

/// The tier a dispatch gets when it names none. Ordinary work, ordinary rung.
pub const DEFAULT_TIER: &str = "terra";

/// The six rungs. Two ladders of three, paired across by standing.
pub const TIERS: [Tier; 6] = [
    Tier {
        name: "luna",
        provider: Provider::Codex,
        model: "gpt-5.6-luna",
        effort: "high",
        counterpart: "sonnet",
    },
    Tier {
        name: "terra",
        provider: Provider::Codex,
        model: "gpt-5.6-terra",
        effort: "xhigh",
        counterpart: "opus",
    },
    Tier {
        name: "sol",
        provider: Provider::Codex,
        model: "gpt-5.6-sol",
        effort: "xhigh",
        counterpart: "fable",
    },
    Tier {
        name: "sonnet",
        provider: Provider::Claude,
        model: "claude-sonnet-5",
        effort: "high",
        counterpart: "luna",
    },
    Tier {
        name: "opus",
        provider: Provider::Claude,
        model: "claude-opus-5",
        effort: "xhigh",
        counterpart: "terra",
    },
    Tier {
        name: "fable",
        provider: Provider::Claude,
        model: "claude-fable-5",
        effort: "xhigh",
        counterpart: "sol",
    },
];

/// Look one rung up by name. An unknown name is a hard error, never a
/// fallback: a dispatcher that misspells a tier must hear about it before a
/// worker is launched, not read it off an attempt afterwards.
pub fn tier(name: &str) -> Result<&'static Tier> {
    TIERS.iter().find(|tier| tier.name == name).ok_or_else(|| {
        DriverError::new(format!(
            "unknown tier `{name}`; the rungs are {}",
            names().join(", ")
        ))
    })
}

pub fn names() -> Vec<&'static str> {
    TIERS.iter().map(|tier| tier.name).collect()
}

impl Tier {
    /// The rung of equal standing on the other ladder.
    pub fn counterpart(&self) -> &'static Tier {
        tier(self.counterpart).expect("every counterpart is a rung of the table")
    }

    /// The engine invocation, one shell word per element, with the goal as the
    /// final argument. Nothing here is typed into a terminal: the words become
    /// argv, so a goal containing quotes, semicolons or the word `Enter` is
    /// just a string.
    ///
    /// `git_common_dir` is the dispatching project's own `.git`, which a
    /// codex worker needs as a second writable root — see [`writable_roots`].
    pub fn command(&self, goal: &str, git_common_dir: Option<&str>) -> Vec<String> {
        let mut words: Vec<String> = match self.provider {
            // approval_policy=never and workspace-write let the worker commit
            // on its branch unattended; network access lets it reach the log
            // through `alder`, which pushes to the store remote.
            Provider::Codex => [
                "codex",
                "exec",
                "-m",
                self.model,
                "-c",
                &format!("model_reasoning_effort={}", self.effort),
                "-c",
                "approval_policy=never",
                "-c",
                "sandbox_mode=workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                &writable_roots(git_common_dir),
            ]
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
            Provider::Claude => [
                "claude",
                "--model",
                self.model,
                "--effort",
                self.effort,
                "--permission-mode",
                "auto",
            ]
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
        };
        words.push(goal.to_owned());
        words
    }

    /// The script that relays a ruling back into a one-shot worker, or `None`
    /// for a provider whose workers sit at an interactive prompt and are
    /// simply typed at.
    ///
    /// `codex exec resume` inherits *nothing* from the session it resumes: no
    /// model, no reasoning effort, no sandbox. Resuming a luna worker with a
    /// bare `codex exec resume <id> "<ruling>"` silently continues it at
    /// whatever model the CLI defaults to — it says so, in a warning nobody is
    /// watching for — with a sandbox that has neither network access nor the
    /// git common dir, so the resumed worker can neither commit nor append.
    ///
    /// So the flags are not documented for a leader to retype. They are
    /// written into the worktree at spawn, by the same table that built the
    /// launch, and the relay is one short command.
    pub fn resume_script(&self, git_common_dir: Option<&str>) -> Option<String> {
        if self.provider != Provider::Codex {
            return None;
        }
        let mut words = self.command("", git_common_dir);
        words.pop(); // the empty goal
        // `codex exec` becomes `codex exec resume "$session"`; the rest of the
        // invocation is repeated exactly as the worker was launched with.
        let flags: Vec<String> = words
            .drain(2..)
            .map(|word| crate::effects::quote(&word))
            .collect();
        Some(format!(
            r#"#!/bin/sh
# Resume this worker's codex session with a ruling from the leader.
#
#     .alder/resume <codex-session-id> "<the ruling>"
#
# A session ID is mandatory. `--last` is unsafe here: a worker may have run a
# consult in this directory, and that would resume the consult instead of this
# worker. The attempt's `codex-session` metadata is the exact answer.
#
# `codex exec resume` inherits nothing from the session it resumes, so the
# model, the effort and the sandbox are repeated here exactly as this worker
# was spawned with them ({tier}: {model}, effort {effort}). A resume without
# them runs at another model's default, cannot commit, and cannot reach the
# log.
set -eu
if [ $# -ne 2 ]; then
  echo "usage: .alder/resume <codex-session-id> <ruling>" >&2
  exit 64
fi
session=$1
shift
exec codex exec resume "$session" {flags} "$1"
"#,
            tier = self.name,
            model = self.model,
            effort = self.effort,
            flags = flags.join(" "),
        ))
    }

    /// A launcher-owned watcher for a Codex worker's session ID.
    ///
    /// `CODEX_THREAD_ID` is only available *inside* the Codex turn, after the
    /// pane has already been created. Putting the stamp in the brief therefore
    /// loses exactly the workers that die before their first tool call. This
    /// watcher starts before `codex exec`, snapshots the existing rollouts,
    /// and claims the first new rollout whose session metadata names this
    /// worktree. It is outside the sandbox and independent of the model's
    /// progress. The marker lets reconciliation surface a repair if its log
    /// append loses a race or the ledger is temporarily unavailable.
    pub fn codex_session_stamp_script(&self) -> Option<&'static str> {
        (self.provider == Provider::Codex).then_some(CODEX_SESSION_STAMP_SCRIPT)
    }
}

/// Starts a detached watcher rather than waiting for Codex to boot. The
/// session files are the local source of truth that `codex exec resume` uses,
/// and session_meta gives both the stable UUID and the worker's cwd. `jq` is
/// already part of the operator environment that runs the tmux observer.
const CODEX_SESSION_STAMP_SCRIPT: &str = r#"#!/usr/bin/env bash
# Stamp a Codex worker attempt without relying on the worker reaching a tool
# call. Invoked by the pane immediately before `codex exec`.
set -uo pipefail

attempt=${ALDER_ATTEMPT:?ALDER_ATTEMPT is required}
worktree=$(pwd -P)
codex_home=${CODEX_HOME:-"$HOME/.codex"}
sessions="$codex_home/sessions"
stamp_dir=.alder
marker="$stamp_dir/codex-session"
log="$stamp_dir/codex-session-stamp.log"

mkdir -p "$stamp_dir"
snapshot=$(mktemp "${TMPDIR:-/tmp}/alder-codex-sessions.XXXXXX")
if [ -d "$sessions" ]; then
  find "$sessions" -type f -name '*.jsonl' -print 2>/dev/null >"$snapshot" || true
else
  : >"$snapshot"
fi

find_new_session() {
  local file session_id
  [ -d "$sessions" ] || return 1
  while IFS= read -r file; do
    grep -Fqx -- "$file" "$snapshot" && continue
    session_id=$(jq -er --arg cwd "$worktree" '
      select(.type == "session_meta" and .payload.cwd == $cwd)
      | (.payload.session_id // .payload.id)
      | select(type == "string")
    ' "$file" 2>/dev/null | head -n 1) || continue
    if [[ "$session_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]; then
      printf '%s\n' "$session_id"
      return 0
    fi
  done < <(find "$sessions" -type f -name '*.jsonl' -print 2>/dev/null)
  return 1
}

(
  trap 'rm -f "$snapshot"' EXIT
  for _ in {1..60}; do
    if session_id=$(find_new_session); then
      temporary="$marker.$$.tmp"
      printf '%s\n' "$session_id" >"$temporary"
      mv -f "$temporary" "$marker"
      for _ in {1..60}; do
        if .alder/bin/alder attempt edit "$attempt" --meta "codex-session=$session_id"; then
          exit 0
        fi
        sleep 1
      done
      printf 'could not stamp %s on %s after 60 attempts\n' "$session_id" "$attempt" >&2
      exit 1
    fi
    sleep 1
  done
  printf 'no new Codex session for %s appeared within 60 seconds\n' "$worktree" >&2
) </dev/null >>"$log" 2>&1 &
disown || true
"#;

/// The second writable root a codex worker cannot commit without.
///
/// A worker lives in a linked git worktree, whose `.git` is a *file* pointing
/// into the dispatching project's `.git/worktrees/<name>`. The index, the
/// objects and the branch ref all live over there, outside the sandbox's
/// workspace, so a `workspace-write` worker that is given only its own
/// checkout fails on the first commit with
/// `Unable to create '…/index.lock': Operation not permitted`. Naming the
/// common dir writable fixes exactly that and nothing else: the leader's
/// working tree stays read-only to the worker, which is the part that matters.
fn writable_roots(git_common_dir: Option<&str>) -> String {
    let roots: Vec<&str> = git_common_dir.into_iter().collect();
    format!(
        "sandbox_workspace_write.writable_roots={}",
        // Serialized rather than interpolated: the value is parsed as TOML,
        // and a path is not guaranteed to be free of characters that matter
        // there. JSON string escaping is TOML string escaping for anything a
        // path can contain.
        serde_json::to_string(&roots).expect("a list of strings serializes")
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn every_rung_pins_a_model_and_an_effort() {
        assert_eq!(names(), ["luna", "terra", "sol", "sonnet", "opus", "fable"]);
        for rung in &TIERS {
            assert!(!rung.model.is_empty(), "{} has no model", rung.name);
            assert!(!rung.effort.is_empty(), "{} has no effort", rung.name);
            // The command carries both, so neither can be left to the CLI.
            let command = rung.command("goal", Some("/projects/alder/.git"));
            assert!(
                command.iter().any(|word| word.contains(rung.model)),
                "{} does not pass its model: {command:?}",
                rung.name
            );
            assert!(
                command.iter().any(|word| word.contains(rung.effort)),
                "{} does not pass its effort: {command:?}",
                rung.name
            );
            assert_eq!(
                command.last().map(String::as_str),
                Some("goal"),
                "{} does not end with the goal",
                rung.name
            );
        }
    }

    #[test]
    fn the_pinned_table_is_the_approved_one() {
        let table: Vec<_> = TIERS
            .iter()
            .map(|rung| {
                (
                    rung.name,
                    rung.provider.as_str(),
                    rung.model,
                    rung.effort,
                    rung.counterpart,
                )
            })
            .collect();
        assert_eq!(
            table,
            [
                ("luna", "codex", "gpt-5.6-luna", "high", "sonnet"),
                ("terra", "codex", "gpt-5.6-terra", "xhigh", "opus"),
                ("sol", "codex", "gpt-5.6-sol", "xhigh", "fable"),
                ("sonnet", "claude", "claude-sonnet-5", "high", "luna"),
                ("opus", "claude", "claude-opus-5", "xhigh", "terra"),
                ("fable", "claude", "claude-fable-5", "xhigh", "sol"),
            ]
        );
        assert_eq!(DEFAULT_TIER, "terra");
        assert!(tier(DEFAULT_TIER).is_ok());
    }

    #[test]
    fn an_unknown_tier_is_an_error_that_names_the_rungs() {
        for name in ["", "gpt-5.6-luna", "Luna", "sonnet-5", "haiku"] {
            let error = tier(name).expect_err("unknown tiers are rejected");
            assert!(error.message.contains(name), "{error}");
            for rung in names() {
                assert!(error.message.contains(rung), "{error} omits {rung}");
            }
        }
    }

    #[test]
    fn counterparts_pair_the_ladders_across_providers() {
        for rung in &TIERS {
            let other = rung.counterpart();
            assert_ne!(
                rung.provider, other.provider,
                "{} falls back within its own provider",
                rung.name
            );
            assert_eq!(
                other.counterpart().name,
                rung.name,
                "{} and {} do not pair",
                rung.name,
                other.name
            );
        }
    }

    #[test]
    fn each_provider_runs_its_own_cli_with_the_pinned_policy() {
        let luna = tier("luna")
            .unwrap()
            .command("do the thing", Some("/projects/alder/.git"));
        assert_eq!(
            luna,
            [
                "codex",
                "exec",
                "-m",
                "gpt-5.6-luna",
                "-c",
                "model_reasoning_effort=high",
                "-c",
                "approval_policy=never",
                "-c",
                "sandbox_mode=workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                r#"sandbox_workspace_write.writable_roots=["/projects/alder/.git"]"#,
                "do the thing",
            ]
        );
        let opus = tier("opus")
            .unwrap()
            .command("do the thing", Some("/projects/alder/.git"));
        assert_eq!(
            opus,
            [
                "claude",
                "--model",
                "claude-opus-5",
                "--effort",
                "xhigh",
                "--permission-mode",
                "auto",
                "do the thing",
            ]
        );
    }

    #[test]
    fn a_codex_rung_writes_a_resume_that_repeats_its_whole_invocation() {
        let script = tier("luna")
            .unwrap()
            .resume_script(Some("/projects/alder/.git"))
            .expect("codex rungs relay by resuming");
        assert!(script.starts_with("#!/bin/sh"), "{script}");
        assert!(
            script.contains("codex exec resume \"$session\""),
            "{script}"
        );
        // Everything the launch pinned, pinned again: resume inherits none of
        // it, and a resumed worker at the wrong model or without the sandbox
        // roots is worse than no relay at all.
        for flag in [
            "'-m' 'gpt-5.6-luna'",
            "'model_reasoning_effort=high'",
            "'approval_policy=never'",
            "'sandbox_mode=workspace-write'",
            "'sandbox_workspace_write.network_access=true'",
            r#"'sandbox_workspace_write.writable_roots=["/projects/alder/.git"]'"#,
        ] {
            assert!(script.contains(flag), "the resume drops {flag}: {script}");
        }
        // The goal placeholder never reaches it; the ruling does.
        assert!(script.trim_end().ends_with("\"$1\""), "{script}");
        assert!(!script.contains("''"), "an empty goal leaked in: {script}");
        assert!(
            script.contains("if [ $# -ne 2 ]; then"),
            "a bare resume must fail: {script}"
        );
        assert!(
            !script.contains("session=--last"),
            "a resume must never guess from the newest session: {script}"
        );

        // A claude worker sits at a prompt and is typed at, so there is
        // nothing to write.
        assert!(tier("opus").unwrap().resume_script(None).is_none());
    }

    #[test]
    fn a_codex_launch_gets_a_sidecar_that_stamps_before_the_worker_can_act() {
        let watcher = tier("terra")
            .unwrap()
            .codex_session_stamp_script()
            .expect("codex launches need a session watcher");
        assert!(watcher.contains("find_new_session"), "{watcher}");
        assert!(watcher.contains(".payload.cwd == $cwd"), "{watcher}");
        assert!(
            watcher.contains("marker=\"$stamp_dir/codex-session\""),
            "{watcher}"
        );
        assert!(
            watcher.contains("attempt edit \"$attempt\" --meta \"codex-session=$session_id\""),
            "the watcher never appends its stamp: {watcher}"
        );
        assert!(
            watcher.find("snapshot=$(mktemp").unwrap()
                < watcher.find(") </dev/null >>\"$log\" 2>&1 &").unwrap(),
            "the snapshot must happen before the detached watcher can see Codex: {watcher}"
        );
        assert!(tier("opus").unwrap().codex_session_stamp_script().is_none());
    }

    #[test]
    fn a_bare_resume_refuses_without_starting_codex() {
        let temporary = TempDir::new().unwrap();
        let resume = temporary.path().join("resume");
        fs::write(
            &resume,
            tier("luna")
                .unwrap()
                .resume_script(Some("/projects/alder/.git"))
                .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&resume, fs::Permissions::from_mode(0o755)).unwrap();

        let bin = temporary.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let called = temporary.path().join("called");
        let codex = bin.join("codex");
        fs::write(
            &codex,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\n",
                called.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();

        let bare = Command::new(&resume)
            .arg("the ruling")
            .env("PATH", &bin)
            .output()
            .unwrap();
        assert!(!bare.status.success());
        assert!(
            String::from_utf8_lossy(&bare.stderr).contains("usage:"),
            "{}",
            String::from_utf8_lossy(&bare.stderr)
        );
        assert!(
            !called.exists(),
            "a bare resume must not invoke the codex command"
        );

        let resumed = Command::new(&resume)
            .args(["019fb2ef-d507-7201-bc36-79d6d5b82336", "the ruling"])
            .env("PATH", &bin)
            .output()
            .unwrap();
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert_eq!(
            fs::read_to_string(called).unwrap(),
            "exec resume 019fb2ef-d507-7201-bc36-79d6d5b82336 -m gpt-5.6-luna -c model_reasoning_effort=high -c approval_policy=never -c sandbox_mode=workspace-write -c sandbox_workspace_write.network_access=true -c sandbox_workspace_write.writable_roots=[\"/projects/alder/.git\"] the ruling\n"
        );
    }

    #[test]
    fn providers_round_trip_by_name() {
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.as_str()).unwrap(), provider);
        }
        assert!(Provider::parse("openai").is_err());
    }
}
