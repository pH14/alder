//! The dispatch ladder: named rungs, each pinning a model and an effort.
//!
//! A tier name is the only thing a caller ever types. Everything the engine is
//! actually run with — provider, model, reasoning effort, sandbox and approval
//! policy — is pinned here, in one table, so that "which model did this run
//! on" is answered by the launch rather than by whatever the CLI's own default
//! happened to be that week.
//!
//! That is the whole reason an unknown tier is a hard error. Falling through
//! to a CLI default would launch an execution at an unknown model and an
//! unknown effort, record nothing about it, and look exactly like a
//! successful start.
//!
//! The built-in table below is the default; a machine-local config file may
//! replace it — see [`crate::config`].

use crate::error::{Result, RunnerError};

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

    /// Both providers, in ladder order. Used by `alder-ext-runner budget` so
    /// it reports every provider whether or not anything was spent on it.
    pub const ALL: [Provider; 2] = [Provider::Codex, Provider::Claude];

    pub fn parse(name: &str) -> Result<Self> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.as_str() == name)
            .ok_or_else(|| {
                RunnerError::new(format!(
                    "unknown provider `{name}`; known providers are codex and claude"
                ))
            })
    }
}

/// How much of the filesystem a codex rung's executions may write.
///
/// This is a property of the rung, not of a launch: the rung is where an
/// operator decides how far to trust the executions it starts, and the resume
/// script must repeat the launch's setting exactly, so both read it from the
/// same table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sandbox {
    /// The execution writes its own worktree and the launching repository's
    /// `.git`, nothing else. The right setting for workers.
    #[default]
    WorkspaceWrite,
    /// No sandbox at all: the execution writes anywhere the operator can.
    /// Exists for the executor rung, which must write the shared log through
    /// the primary checkout and launch nested tool processes.
    FullAccess,
}

impl Sandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::WorkspaceWrite => "workspace-write",
            Sandbox::FullAccess => "full-access",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        [Sandbox::WorkspaceWrite, Sandbox::FullAccess]
            .into_iter()
            .find(|sandbox| sandbox.as_str() == name)
            .ok_or_else(|| {
                RunnerError::new(format!(
                    "unknown sandbox `{name}`; the settings are workspace-write and full-access"
                ))
            })
    }
}

/// One rung of the ladder.
#[derive(Debug)]
pub struct Tier {
    pub name: &'static str,
    pub provider: Provider,
    /// The full model name, as it is passed to the CLI. Never an alias: an
    /// alias moves under us.
    pub model: &'static str,
    pub effort: &'static str,
    /// The rung of equal standing on the other provider's ladder, used when
    /// this one's provider is rate-limited.
    pub counterpart: &'static str,
    /// How far this rung's codex executions may write. Meaningless for claude
    /// rungs, and the config loader refuses a claude rung that names one.
    pub sandbox: Sandbox,
}

/// The built-in table: two ladders of three, paired across by standing.
pub const TIERS: [Tier; 6] = [
    Tier {
        name: "luna",
        provider: Provider::Codex,
        model: "gpt-5.6-luna",
        effort: "high",
        counterpart: "sonnet",
        sandbox: Sandbox::WorkspaceWrite,
    },
    Tier {
        name: "terra",
        provider: Provider::Codex,
        model: "gpt-5.6-terra",
        effort: "xhigh",
        counterpart: "opus",
        sandbox: Sandbox::WorkspaceWrite,
    },
    Tier {
        name: "sol",
        provider: Provider::Codex,
        model: "gpt-5.6-sol",
        effort: "xhigh",
        counterpart: "fable",
        sandbox: Sandbox::WorkspaceWrite,
    },
    Tier {
        name: "sonnet",
        provider: Provider::Claude,
        model: "claude-sonnet-5",
        effort: "high",
        counterpart: "luna",
        sandbox: Sandbox::WorkspaceWrite,
    },
    Tier {
        name: "opus",
        provider: Provider::Claude,
        model: "claude-opus-5",
        effort: "xhigh",
        counterpart: "terra",
        sandbox: Sandbox::WorkspaceWrite,
    },
    Tier {
        name: "fable",
        provider: Provider::Claude,
        model: "claude-fable-5",
        effort: "xhigh",
        counterpart: "sol",
        sandbox: Sandbox::WorkspaceWrite,
    },
];

/// Look one rung up by name in the active table. An unknown name is a hard
/// error, never a fallback: a caller that misspells a tier must hear about it
/// before an execution is launched, not read it off a transcript afterwards.
pub fn lookup<'table>(table: &'table [Tier], name: &str) -> Result<&'table Tier> {
    table.iter().find(|tier| tier.name == name).ok_or_else(|| {
        RunnerError::new(format!(
            "unknown tier `{name}`; the rungs are {}",
            names(table).join(", ")
        ))
    })
}

pub fn names(table: &[Tier]) -> Vec<&str> {
    table.iter().map(|tier| tier.name).collect()
}

impl Tier {
    /// The rung of equal standing on the other ladder. The config loader
    /// validates the pairing, so a table in use always resolves.
    pub fn counterpart<'table>(&self, table: &'table [Tier]) -> &'table Tier {
        lookup(table, self.counterpart).expect("every counterpart is a rung of the table")
    }

    /// The engine invocation, one shell word per element, with the prompt as
    /// the final argument. Nothing here is typed into a terminal: the words
    /// become argv, so a prompt containing quotes, semicolons or the word
    /// `Enter` is just a string.
    ///
    /// `git_common_dir` is the launching repository's own `.git`, which a
    /// codex execution needs as a second writable root — see [`writable_roots`].
    pub fn command(&self, prompt: &str, git_common_dir: Option<&str>) -> Vec<String> {
        let mut words: Vec<String> = match self.provider {
            // approval_policy=never lets the execution act unattended; the
            // rung's sandbox setting decides how far its writes reach.
            Provider::Codex => {
                let mut words: Vec<String> = [
                    "codex",
                    "exec",
                    "-m",
                    self.model,
                    "-c",
                    &format!("model_reasoning_effort={}", self.effort),
                    "-c",
                    "approval_policy=never",
                ]
                .iter()
                .map(|word| (*word).to_owned())
                .collect();
                match self.sandbox {
                    // workspace-write lets the execution commit on its branch
                    // and nothing more; network access lets it reach whatever
                    // remotes its work needs.
                    Sandbox::WorkspaceWrite => words.extend(
                        [
                            "-c",
                            "sandbox_mode=workspace-write",
                            "-c",
                            "sandbox_workspace_write.network_access=true",
                            "-c",
                            &writable_roots(git_common_dir),
                        ]
                        .iter()
                        .map(|word| (*word).to_owned()),
                    ),
                    // No sandbox, so no sandbox_workspace_write.* keys:
                    // writable roots are meaningless when nothing confines
                    // the writes.
                    Sandbox::FullAccess => words.extend(
                        ["-c", "sandbox_mode=danger-full-access"]
                            .iter()
                            .map(|word| (*word).to_owned()),
                    ),
                }
                words
            }
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
        words.push(prompt.to_owned());
        words
    }

    /// The script that resumes a one-shot execution with a later message, or
    /// `None` for a provider whose executions sit at an interactive prompt and
    /// are simply typed at.
    ///
    /// `codex exec resume` inherits *nothing* from the session it resumes: no
    /// model, no reasoning effort, no sandbox. Resuming a luna execution with a
    /// bare `codex exec resume <id> "<message>"` silently continues it at
    /// whatever model the CLI defaults to — it says so, in a warning nobody is
    /// watching for — with a sandbox that has neither network access nor the
    /// git common dir, so the resumed execution can commit nothing.
    ///
    /// So the flags are not documented for anyone to retype. They are written
    /// into the runner's per-handle state directory at start, by the same
    /// table that built the launch, and `send` runs the script for the caller.
    pub fn resume_script(&self, git_common_dir: Option<&str>) -> Option<String> {
        if self.provider != Provider::Codex {
            return None;
        }
        let mut words = self.command("", git_common_dir);
        words.pop(); // the empty prompt
        // `codex exec` becomes `codex exec resume "$session"`; the rest of the
        // invocation is repeated exactly as the execution was launched with.
        let flags: Vec<String> = words
            .drain(2..)
            .map(|word| crate::host::quote(&word))
            .collect();
        Some(format!(
            r#"#!/bin/sh
# Resume this execution's codex session with a later message.
#
#     resume <codex-session-id> "<the message>"
#
# This script lives in the runner's per-handle state directory, beside the
# `codex-session` marker. A session ID is mandatory. `--last` is unsafe here:
# something else may have run codex in this directory, and that would resume
# the wrong session. The `codex-session` marker is the exact answer.
#
# `codex exec resume` inherits nothing from the session it resumes, so the
# model, the effort and the sandbox are repeated here exactly as this
# execution was started with them ({tier}: {model}, effort {effort}). A resume
# without them runs at another model's default and cannot commit.
set -eu
if [ $# -ne 2 ]; then
  echo "usage: resume <codex-session-id> <message>" >&2
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

    /// A launcher-owned watcher for a Codex execution's session ID.
    ///
    /// `CODEX_THREAD_ID` is only available *inside* the Codex turn, after the
    /// pane has already been created. Asking the model to report it therefore
    /// loses exactly the executions that die before their first tool call.
    /// This watcher starts before `codex exec`, snapshots the existing
    /// rollouts, and claims the first new rollout whose session metadata names
    /// this worktree, leaving the ID in the per-handle state directory's
    /// `codex-session` marker for `send` to resume with. It is outside the
    /// sandbox and independent of the model's progress.
    pub fn codex_session_stamp_script(&self) -> Option<&'static str> {
        (self.provider == Provider::Codex).then_some(CODEX_SESSION_STAMP_SCRIPT)
    }
}

/// Starts a detached watcher rather than waiting for Codex to boot. The
/// session files are the local source of truth that `codex exec resume` uses,
/// and session_meta gives both the stable UUID and the execution's cwd. `jq`
/// is part of the operator environment the runner already assumes.
const CODEX_SESSION_STAMP_SCRIPT: &str = r#"#!/usr/bin/env bash
# Record this Codex execution's session ID without relying on the model
# reaching a tool call. Invoked by the pane immediately before `codex exec`.
#
# The marker is written next to this script, in the runner-owned per-handle
# state directory — never into the worktree, whose contents the execution
# itself controls.
set -uo pipefail

worktree=$(pwd -P)
codex_home=${CODEX_HOME:-"$HOME/.codex"}
sessions="$codex_home/sessions"
stamp_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
marker="$stamp_dir/codex-session"
log="$stamp_dir/codex-session-stamp.log"

mkdir -p "$stamp_dir"
snapshot=$(mktemp "${TMPDIR:-/tmp}/alder-ext-codex-sessions.XXXXXX")
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
      exit 0
    fi
    sleep 1
  done
  printf 'no new Codex session for %s appeared within 60 seconds\n' "$worktree" >&2
) </dev/null >>"$log" 2>&1 &
disown || true
"#;

/// The second writable root a codex execution cannot commit without.
///
/// An execution lives in a linked git worktree, whose `.git` is a *file*
/// pointing into the launching repository's `.git/worktrees/<name>`. The
/// index, the objects and the branch ref all live over there, outside the
/// sandbox's workspace, so a `workspace-write` execution that is given only
/// its own checkout fails on the first commit with
/// `Unable to create '…/index.lock': Operation not permitted`. Naming the
/// common dir writable fixes exactly that and nothing else: the launching
/// repository's working tree stays read-only to the execution, which is the
/// part that matters.
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

    fn tier(name: &str) -> &'static Tier {
        lookup(&TIERS, name).expect("a built-in rung")
    }

    #[test]
    fn every_rung_pins_a_model_and_an_effort() {
        assert_eq!(
            names(&TIERS),
            ["luna", "terra", "sol", "sonnet", "opus", "fable"]
        );
        for rung in &TIERS {
            assert!(!rung.model.is_empty(), "{} has no model", rung.name);
            assert!(!rung.effort.is_empty(), "{} has no effort", rung.name);
            // Every built-in rung is a worker rung: full access is granted
            // only by an operator's own config, never by the default table.
            assert_eq!(
                rung.sandbox,
                Sandbox::WorkspaceWrite,
                "{} escalates the built-in table",
                rung.name
            );
            // The command carries both, so neither can be left to the CLI.
            let command = rung.command("prompt", Some("/projects/alder/.git"));
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
                Some("prompt"),
                "{} does not end with the prompt",
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
    }

    #[test]
    fn an_unknown_tier_is_an_error_that_names_the_rungs() {
        for name in ["", "gpt-5.6-luna", "Luna", "sonnet-5", "haiku"] {
            let error = lookup(&TIERS, name).expect_err("unknown tiers are rejected");
            assert!(error.message.contains(name), "{error}");
            for rung in names(&TIERS) {
                assert!(error.message.contains(rung), "{error} omits {rung}");
            }
        }
    }

    #[test]
    fn counterparts_pair_the_ladders_across_providers() {
        for rung in &TIERS {
            let other = rung.counterpart(&TIERS);
            assert_ne!(
                rung.provider, other.provider,
                "{} falls back within its own provider",
                rung.name
            );
            assert_eq!(
                other.counterpart(&TIERS).name,
                rung.name,
                "{} and {} do not pair",
                rung.name,
                other.name
            );
        }
    }

    #[test]
    fn each_provider_runs_its_own_cli_with_the_pinned_policy() {
        let luna = tier("luna").command("do the thing", Some("/projects/alder/.git"));
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
        let opus = tier("opus").command("do the thing", Some("/projects/alder/.git"));
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
    fn a_full_access_rung_drops_the_sandbox_from_launch_and_resume_alike() {
        let executor = Tier {
            name: "executor",
            provider: Provider::Codex,
            model: "gpt-5.6-terra",
            effort: "xhigh",
            counterpart: "opus",
            sandbox: Sandbox::FullAccess,
        };
        // No writable_roots word: that key names roots a sandbox would leave
        // writable, and there is no sandbox to leave them.
        assert_eq!(
            executor.command("do the thing", Some("/projects/alder/.git")),
            [
                "codex",
                "exec",
                "-m",
                "gpt-5.6-terra",
                "-c",
                "model_reasoning_effort=xhigh",
                "-c",
                "approval_policy=never",
                "-c",
                "sandbox_mode=danger-full-access",
                "do the thing",
            ]
        );
        // The resume must continue the session exactly as it was launched: a
        // full-access execution resumed under workspace-write would fail the
        // same writes its rung exists to allow.
        let script = executor
            .resume_script(Some("/projects/alder/.git"))
            .expect("codex rungs resume");
        assert!(
            script.contains("'sandbox_mode=danger-full-access'"),
            "{script}"
        );
        assert!(script.contains("'approval_policy=never'"), "{script}");
        for stale in [
            "writable_roots",
            "sandbox_workspace_write",
            "workspace-write",
        ] {
            assert!(
                !script.contains(stale),
                "the resume smuggles the workspace sandbox back in via `{stale}`: {script}"
            );
        }
    }

    #[test]
    fn sandbox_settings_round_trip_by_name() {
        for sandbox in [Sandbox::WorkspaceWrite, Sandbox::FullAccess] {
            assert_eq!(Sandbox::parse(sandbox.as_str()).unwrap(), sandbox);
        }
        assert_eq!(Sandbox::default(), Sandbox::WorkspaceWrite);
        // The config word is the runner's own name for the setting, never
        // codex's flag value: accepting `danger-full-access` here would let a
        // typo of intent pass as the most dangerous setting.
        for name in ["danger-full-access", "none", "Full-Access", ""] {
            let error = Sandbox::parse(name).expect_err("unknown sandboxes are rejected");
            assert!(error.message.contains("workspace-write"), "{error}");
            assert!(error.message.contains("full-access"), "{error}");
        }
    }

    #[test]
    fn a_codex_rung_writes_a_resume_that_repeats_its_whole_invocation() {
        let script = tier("luna")
            .resume_script(Some("/projects/alder/.git"))
            .expect("codex rungs resume");
        assert!(script.starts_with("#!/bin/sh"), "{script}");
        assert!(
            script.contains("codex exec resume \"$session\""),
            "{script}"
        );
        // Everything the launch pinned, pinned again: resume inherits none of
        // it, and a resumed execution at the wrong model or without the
        // sandbox roots is worse than no resume at all.
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
        // The prompt placeholder never reaches it; the message does.
        assert!(script.trim_end().ends_with("\"$1\""), "{script}");
        assert!(
            !script.contains("''"),
            "an empty prompt leaked in: {script}"
        );
        assert!(
            script.contains("if [ $# -ne 2 ]; then"),
            "a bare resume must fail: {script}"
        );
        assert!(
            !script.contains("session=--last"),
            "a resume must never guess from the newest session: {script}"
        );

        // A claude execution sits at a prompt and is typed at, so there is
        // nothing to write.
        assert!(tier("opus").resume_script(None).is_none());
    }

    #[test]
    fn a_codex_launch_gets_a_sidecar_that_stamps_before_the_model_can_act() {
        let watcher = tier("terra")
            .codex_session_stamp_script()
            .expect("codex launches need a session watcher");
        assert!(watcher.contains("find_new_session"), "{watcher}");
        assert!(watcher.contains(".payload.cwd == $cwd"), "{watcher}");
        assert!(
            watcher.contains("marker=\"$stamp_dir/codex-session\""),
            "{watcher}"
        );
        assert!(
            watcher.contains(r#"stamp_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"#),
            "the marker must live beside the script, in the runner-owned state \
             directory rather than the worker-writable worktree: {watcher}"
        );
        assert!(
            watcher.find("snapshot=$(mktemp").unwrap()
                < watcher.find(") </dev/null >>\"$log\" 2>&1 &").unwrap(),
            "the snapshot must happen before the detached watcher can see Codex: {watcher}"
        );
        // The runner knows nothing that is not its own: the sidecar records
        // its marker locally and calls no other tool to announce it.
        assert!(
            !watcher.contains("alder "),
            "the sidecar reaches outside the runner: {watcher}"
        );
        assert!(tier("opus").codex_session_stamp_script().is_none());
    }

    #[test]
    fn a_bare_resume_refuses_without_starting_codex() {
        let temporary = TempDir::new().unwrap();
        let resume = temporary.path().join("resume");
        fs::write(
            &resume,
            tier("luna")
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
            .arg("the message")
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
            .args(["019fb2ef-d507-7201-bc36-79d6d5b82336", "the message"])
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
            "exec resume 019fb2ef-d507-7201-bc36-79d6d5b82336 -m gpt-5.6-luna -c model_reasoning_effort=high -c approval_policy=never -c sandbox_mode=workspace-write -c sandbox_workspace_write.network_access=true -c sandbox_workspace_write.writable_roots=[\"/projects/alder/.git\"] the message\n"
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
