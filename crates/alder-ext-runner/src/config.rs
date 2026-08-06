//! The runner's machine-local configuration.
//!
//! Which models exist and what they cost are properties of the box the runner
//! runs on, not facts about any repository it launches executions for, so the
//! config lives in the operator's home rather than in a project:
//!
//! - the config file is `$ALDER_EXT_RUNNER_CONFIG`, or
//!   `~/.config/alder-ext-runner/config.json`; a missing file means the
//!   built-in tier table;
//! - rate-limit state lives under `$ALDER_EXT_RUNNER_STATE_DIR`, or
//!   `~/.local/state/alder-ext-runner/`.
//!
//! The config file's whole format is the tier table (see the crate README):
//!
//! ```json
//! {
//!   "tiers": {
//!     "luna": {
//!       "provider": "codex",
//!       "model": "gpt-5.6-luna",
//!       "effort": "high",
//!       "counterpart": "sonnet"
//!     }
//!   }
//! }
//! ```
//!
//! The engine command per provider — how `codex exec` and `claude` are
//! invoked — is code, not configuration: it is the part of a launch that has
//! to stay in step with the resume script, and the two are generated from one
//! table on purpose. The one knob a codex rung gets is `"sandbox"`:
//! `"workspace-write"` (the default) or `"full-access"`. What each setting
//! expands to is still code.

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::Deserialize;

use crate::{
    error::{Result, RunnerError},
    tier::{Provider, Sandbox, TIERS, Tier},
};

/// Where the config file lives, unless the environment moves it.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ALDER_EXT_RUNNER_CONFIG") {
        return PathBuf::from(path);
    }
    home().join(".config/alder-ext-runner/config.json")
}

/// Where machine-local state (the rate-limit file) lives.
pub fn state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("ALDER_EXT_RUNNER_STATE_DIR") {
        return PathBuf::from(path);
    }
    home().join(".local/state/alder-ext-runner")
}

pub fn limits_path() -> PathBuf {
    state_dir().join("rate-limits.json")
}

/// The runner-owned directory for one handle's machine-local state: the codex
/// resume script, the codex-session marker, and the session watcher's log.
///
/// This lives under the state directory, never inside the worktree, because
/// the worktree is written by the execution itself: anything the runner later
/// trusts or executes must not sit where the worker can rewrite it.
pub fn handle_state_dir(handle: &str) -> PathBuf {
    state_dir().join(handle)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    tiers: Option<BTreeMap<String, TierEntry>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TierEntry {
    provider: String,
    model: String,
    effort: String,
    counterpart: String,
    /// `workspace-write` (the default) or `full-access`. Codex rungs only:
    /// a claude rung naming any sandbox is refused rather than ignored,
    /// because a silently dropped setting reads as granted.
    #[serde(default)]
    sandbox: Option<String>,
}

/// The active tier table: the config file's, or the built-in one when no file
/// exists. The loaded table is leaked once per process, which is what lets
/// the rest of the crate keep borrowing rungs for the life of the run.
pub fn load_tiers() -> Result<&'static [Tier]> {
    let path = config_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(&TIERS),
        Err(error) => {
            return Err(RunnerError::new(format!(
                "cannot read `{}`: {error}",
                path.display()
            )));
        }
    };
    let config: FileConfig = serde_json::from_slice(&bytes)
        .map_err(|error| RunnerError::new(format!("invalid `{}`: {error}", path.display())))?;
    match config.tiers {
        None => Ok(&TIERS),
        Some(entries) => build_table(entries),
    }
}

/// Turn config entries into a leaked, validated table.
fn build_table(entries: BTreeMap<String, TierEntry>) -> Result<&'static [Tier]> {
    if entries.is_empty() {
        return Err(RunnerError::new("the tier table cannot be empty"));
    }
    let mut table = Vec::with_capacity(entries.len());
    for (name, entry) in entries {
        if name.trim().is_empty() {
            return Err(RunnerError::new("a tier name cannot be empty"));
        }
        for (field, value) in [("model", &entry.model), ("effort", &entry.effort)] {
            if value.trim().is_empty() {
                return Err(RunnerError::new(format!("tier `{name}` has no {field}")));
            }
        }
        let provider = Provider::parse(&entry.provider)
            .map_err(|error| RunnerError::new(format!("tier `{name}`: {error}")))?;
        let sandbox = match entry.sandbox {
            None => Sandbox::default(),
            Some(_) if provider == Provider::Claude => {
                return Err(RunnerError::new(format!(
                    "tier `{name}` is a claude rung and cannot set a sandbox; \
                     `sandbox` applies to codex rungs only"
                )));
            }
            Some(value) => Sandbox::parse(&value)
                .map_err(|error| RunnerError::new(format!("tier `{name}`: {error}")))?,
        };
        table.push(Tier {
            provider,
            sandbox,
            name: leak(name),
            model: leak(entry.model),
            effort: leak(entry.effort),
            counterpart: leak(entry.counterpart),
        });
    }
    let table: &'static [Tier] = Box::leak(table.into_boxed_slice());
    validate_pairing(table)?;
    Ok(table)
}

/// Every counterpart must exist, sit on the other provider's ladder, and pair
/// back — [`Tier::counterpart`] relies on it, and a rate-limited dispatch
/// rerouted to a missing rung would fail at the worst moment instead of now.
fn validate_pairing(table: &[Tier]) -> Result<()> {
    for tier in table {
        let other = crate::tier::lookup(table, tier.counterpart).map_err(|_| {
            RunnerError::new(format!(
                "tier `{}` names counterpart `{}`, which is not in the table",
                tier.name, tier.counterpart
            ))
        })?;
        if other.provider == tier.provider {
            return Err(RunnerError::new(format!(
                "tier `{}` and its counterpart `{}` share a provider; a \
                 counterpart is the fallback for a rate-limited provider",
                tier.name, other.name
            )));
        }
        if other.counterpart != tier.name {
            return Err(RunnerError::new(format!(
                "tier `{}` names counterpart `{}`, but `{}` names `{}`",
                tier.name, other.name, other.name, other.counterpart
            )));
        }
    }
    Ok(())
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_from(body: &str) -> Result<&'static [Tier]> {
        let config: FileConfig = serde_json::from_str(body)
            .map_err(|error| RunnerError::new(format!("invalid: {error}")))?;
        match config.tiers {
            None => Ok(&TIERS),
            Some(entries) => build_table(entries),
        }
    }

    #[test]
    fn a_missing_or_empty_config_serves_the_built_in_table() {
        let table = table_from("{}").unwrap();
        assert_eq!(table.len(), TIERS.len());
        assert!(crate::tier::lookup(table, "terra").is_ok());
    }

    #[test]
    fn a_config_table_replaces_the_built_in_one_whole() {
        let table = table_from(
            r#"{"tiers": {
                "fast": {"provider": "codex", "model": "gpt-x", "effort": "low", "counterpart": "steady"},
                "steady": {"provider": "claude", "model": "claude-y", "effort": "high", "counterpart": "fast"}
            }}"#,
        )
        .unwrap();
        assert_eq!(crate::tier::names(table), ["fast", "steady"]);
        let fast = crate::tier::lookup(table, "fast").unwrap();
        assert_eq!(fast.model, "gpt-x");
        assert_eq!(fast.counterpart(table).name, "steady");
        assert!(
            crate::tier::lookup(table, "terra").is_err(),
            "the built-in rungs must not leak through a replacement table"
        );
    }

    #[test]
    fn a_rung_carries_its_sandbox_and_defaults_to_workspace_write() {
        let table = table_from(
            r#"{"tiers": {
                "executor": {"provider": "codex", "model": "gpt-x", "effort": "xhigh", "counterpart": "steady", "sandbox": "full-access"},
                "worker": {"provider": "codex", "model": "gpt-x", "effort": "high", "counterpart": "helper", "sandbox": "workspace-write"},
                "steady": {"provider": "claude", "model": "claude-y", "effort": "high", "counterpart": "executor"},
                "helper": {"provider": "claude", "model": "claude-y", "effort": "high", "counterpart": "worker"}
            }}"#,
        )
        .unwrap();
        let sandbox_of = |name| crate::tier::lookup(table, name).unwrap().sandbox;
        assert_eq!(sandbox_of("executor"), Sandbox::FullAccess);
        assert_eq!(sandbox_of("worker"), Sandbox::WorkspaceWrite);
        // Absent means workspace-write: an existing config keeps exactly the
        // behavior it had before the field existed.
        assert_eq!(sandbox_of("steady"), Sandbox::WorkspaceWrite);
        assert_eq!(sandbox_of("helper"), Sandbox::WorkspaceWrite);
    }

    #[test]
    fn every_invalid_table_is_rejected_by_name() {
        for (body, complaint) in [
            (r#"{"tiers": {}}"#, "cannot be empty"),
            (
                r#"{"tiers": {"a": {"provider": "openai", "model": "m", "effort": "e", "counterpart": "a"}}}"#,
                "unknown provider",
            ),
            (
                r#"{"tiers": {"a": {"provider": "codex", "model": " ", "effort": "e", "counterpart": "a"}}}"#,
                "has no model",
            ),
            (
                r#"{"tiers": {"a": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b"}}}"#,
                "not in the table",
            ),
            (
                r#"{"tiers": {
                    "a": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b"},
                    "b": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "a"}
                }}"#,
                "share a provider",
            ),
            (
                r#"{"tiers": {
                    "a": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b"},
                    "b": {"provider": "claude", "model": "m", "effort": "e", "counterpart": "c"},
                    "c": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b"}
                }}"#,
                "names",
            ),
            (
                // The runner's word is `full-access`; codex's flag value is
                // not a config value, and a typo of intent must not pass as
                // the most dangerous setting.
                r#"{"tiers": {
                    "a": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b", "sandbox": "danger-full-access"},
                    "b": {"provider": "claude", "model": "m", "effort": "e", "counterpart": "a"}
                }}"#,
                "unknown sandbox",
            ),
            (
                // Refused, not ignored: a claude rung's sandbox setting would
                // read as granted while doing nothing.
                r#"{"tiers": {
                    "a": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "b"},
                    "b": {"provider": "claude", "model": "m", "effort": "e", "counterpart": "a", "sandbox": "workspace-write"}
                }}"#,
                "cannot set a sandbox",
            ),
            (r#"{"unknown": 1}"#, "invalid"),
        ] {
            let error = table_from(body).expect_err(body);
            assert!(
                error.message.contains(complaint),
                "`{body}` failed with `{error}`, expected `{complaint}`"
            );
        }
    }

    #[test]
    fn the_paths_follow_the_environment_overrides() {
        // Read-only: this asserts the derivation, not the environment.
        let config = config_path();
        let limits = limits_path();
        assert!(
            config.to_string_lossy().ends_with("config.json"),
            "{config:?}"
        );
        assert!(
            limits.to_string_lossy().ends_with("rate-limits.json"),
            "{limits:?}"
        );
    }
}
