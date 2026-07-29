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
}

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
    fn providers_round_trip_by_name() {
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.as_str()).unwrap(), provider);
        }
        assert!(Provider::parse("openai").is_err());
    }
}
