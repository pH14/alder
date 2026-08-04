//! What the last few hours cost, per provider.
//!
//! Both engines write their own token usage into local transcripts, exactly as
//! the API reported it, so spend can be read off the disk without an API call,
//! an API key, or a running session. `alderd budget` sums it over a trailing
//! window and says whether either provider is currently rate-limited. That is
//! all it does: no caps, no percentages, no thresholds. An executor deciding
//! which rung to dispatch on reads the number and judges.
//!
//! The two providers record different things, so the two halves are honest
//! about measuring different things:
//!
//! - **Codex** writes a `token_count` event per turn. Their `last_token_usage`
//!   fields are per-turn usage, so summing them over a window is that window's
//!   real spend. `total_token_usage` is a running lifetime figure that counts
//!   every cache read again, and is never what this reads.
//! - **Claude Code** writes usage on every assistant entry, cumulative within
//!   the request rather than per-turn. Summing every entry would count one
//!   conversation's cache reads dozens of times, so this takes each session's
//!   *last* assistant entry — the largest true prompt it sent — and sums those
//!   across sessions. Subagent turns (`isSidechain`) are written into the same
//!   file with their own context and are excluded.
//!
//! Both are therefore comparable across time for the same provider, and are
//! not comparable *between* providers. The report says so.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::{error::Result, limits::Limits, tier::Provider};

/// Where Codex keeps its rollouts, overridable the way Codex itself does.
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
}

/// Where Claude Code keeps its transcripts, overridable the same way.
pub fn claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Spend {
    pub sessions: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Spend {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn add(&mut self, input: u64, output: u64) {
        self.input_tokens += input;
        self.output_tokens += output;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub provider: &'static str,
    pub spend: Spend,
    /// How the number was arrived at, so nobody reads it for more than it is.
    pub basis: &'static str,
    /// When the provider is expected to be usable again, if it is limited now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limited_until: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limited_why: Option<String>,
    /// A transcript directory that could not be read, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub hours: i64,
    pub providers: Vec<ProviderReport>,
}

impl Report {
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "trailing {}h to {}",
            self.hours,
            self.until.to_rfc3339()
        )];
        for provider in &self.providers {
            let limit = match (provider.limited_until, &provider.limited_why) {
                (Some(until), Some(why)) => {
                    format!("  rate-limited until {} ({why})", until.to_rfc3339())
                }
                (Some(until), None) => format!("  rate-limited until {}", until.to_rfc3339()),
                _ => String::new(),
            };
            lines.push(format!(
                "{:<7} {:>12} tokens  {:>4} sessions  ({} in, {} out){limit}",
                provider.provider,
                provider.spend.total(),
                provider.spend.sessions,
                provider.spend.input_tokens,
                provider.spend.output_tokens,
            ));
            lines.push(format!("        {}", provider.basis));
            if let Some(unread) = &provider.unread {
                lines.push(format!("        unread: {unread}"));
            }
        }
        lines
    }
}

const CODEX_BASIS: &str =
    "sum of per-turn last_token_usage from ~/.codex/sessions — real spend in the window";
const CLAUDE_BASIS: &str =
    "sum of each session's last assistant usage from ~/.claude/projects — a floor, not real spend";

/// Read spend and rate-limit state for both providers.
pub fn report(
    now: DateTime<Utc>,
    hours: i64,
    limits: &Limits,
    codex: &Path,
    claude: &Path,
) -> Report {
    let since = now - chrono::Duration::hours(hours);
    let providers = Provider::ALL
        .into_iter()
        .map(|provider| {
            let (spend, unread, basis) = match provider {
                Provider::Codex => {
                    let (spend, unread) = codex_spend(codex, since, now);
                    (spend, unread, CODEX_BASIS)
                }
                Provider::Claude => {
                    let (spend, unread) = claude_spend(claude, since, now);
                    (spend, unread, CLAUDE_BASIS)
                }
            };
            let limit = limits.limited(provider, now);
            ProviderReport {
                provider: provider.as_str(),
                spend,
                basis,
                limited_until: limit.map(|limit| limit.until),
                limited_why: limit.and_then(|limit| limit.why.clone()),
                unread,
            }
        })
        .collect();
    Report {
        schema: "alderd.budget.v0",
        since,
        until: now,
        hours,
        providers,
    }
}

/// Every Codex rollout touched since the window opened.
///
/// The files are sharded by the day a session *started*, so a session running
/// since Tuesday is not under today. Modification time is what selects a file
/// — a rollout is appended to on every turn — and event timestamps are what
/// select the turns inside it.
fn codex_spend(home: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> (Spend, Option<String>) {
    let sessions = home.join("sessions");
    let mut spend = Spend::default();
    let mut files = Vec::new();
    // sessions/YYYY/MM/DD/*.jsonl
    let years = match subdirectories(&sessions) {
        Ok(years) => years,
        Err(error) => return (spend, Some(error)),
    };
    for year in years {
        for month in subdirectories(&year).unwrap_or_default() {
            for day in subdirectories(&month).unwrap_or_default() {
                files.extend(transcripts(&day, since));
            }
        }
    }
    for file in files {
        let mut counted = false;
        for event in lines(&file) {
            let Some(at) = timestamp(&event, "timestamp") else {
                continue;
            };
            if at < since || at > until {
                continue;
            }
            let payload = event.get("payload").unwrap_or(&Value::Null);
            if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                continue;
            }
            // `last_token_usage` is this turn; `total_token_usage` is a
            // lifetime figure that would multiply every cache read.
            let Some(usage) = payload.pointer("/info/last_token_usage") else {
                continue;
            };
            // `input_tokens` is the whole prompt for that turn, cached part
            // included — the sibling `cached_input_tokens` is a breakdown of
            // it, not an addition to it.
            spend.add(
                number(usage, "input_tokens"),
                number(usage, "output_tokens"),
            );
            counted = true;
        }
        if counted {
            spend.sessions += 1;
        }
    }
    (spend, None)
}

/// Every Claude Code transcript touched since the window opened, one figure
/// each: the last non-sidechain assistant entry's usage.
fn claude_spend(
    home: &Path,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> (Spend, Option<String>) {
    let projects = home.join("projects");
    let mut spend = Spend::default();
    let directories = match subdirectories(&projects) {
        Ok(directories) => directories,
        Err(error) => return (spend, Some(error)),
    };
    for directory in directories {
        for file in transcripts(&directory, since) {
            // One figure per session: the last assistant turn it took inside
            // the window. An entry with no timestamp is taken as in-window,
            // since the file itself was written into during it.
            let last = lines(&file).into_iter().rfind(|entry| {
                entry.get("type").and_then(Value::as_str) == Some("assistant")
                    && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
                    && entry.pointer("/message/usage").is_some()
                    && timestamp(entry, "timestamp").is_none_or(|at| at >= since && at <= until)
            });
            let Some(entry) = last else { continue };
            let usage = entry.pointer("/message/usage").expect("filtered for it");
            spend.add(
                number(usage, "input_tokens")
                    + number(usage, "cache_creation_input_tokens")
                    + number(usage, "cache_read_input_tokens"),
                number(usage, "output_tokens"),
            );
            spend.sessions += 1;
        }
    }
    (spend, None)
}

fn subdirectories(path: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(path).map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
    let mut directories: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    directories.sort();
    Ok(directories)
}

/// The `.jsonl` files in one directory written into since the window opened.
fn transcripts(directory: &Path, since: DateTime<Utc>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("jsonl")
        })
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| DateTime::<Utc>::from(modified) >= since)
                .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// One transcript, as the JSON objects it parses into. A line that is not JSON
/// is skipped: a transcript being appended to right now can end mid-line.
fn lines(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn timestamp(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Read the report the way `alderd budget` does.
pub fn run(now: DateTime<Utc>, hours: i64, limits: &Limits) -> Result<Report> {
    Ok(report(now, hours, limits, &codex_home(), &claude_home()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::Duration;
    use serde_json::json;

    use super::*;

    /// The fixtures are written now, so their modification times are now: a
    /// window anchored anywhere else would exclude every one of them before a
    /// single line was read.
    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn at(now: DateTime<Utc>, minutes_ago: i64) -> String {
        (now - Duration::minutes(minutes_ago)).to_rfc3339()
    }

    fn write(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn codex_turn(when: &str, input: u64, output: u64) -> Value {
        json!({
            "timestamp": when,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {"input_tokens": 9_000_000, "output_tokens": 9_000_000},
                    "last_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": input,
                        "cache_write_input_tokens": 0,
                        "output_tokens": output,
                    },
                    "model_context_window": 258_400,
                },
            },
        })
    }

    fn claude_turn(when: &str, sidechain: bool, input: u64, output: u64) -> Value {
        json!({
            "type": "assistant",
            "isSidechain": sidechain,
            "timestamp": when,
            "message": {
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": output,
                },
            },
        })
    }

    #[test]
    fn codex_spend_sums_per_turn_usage_inside_the_window() {
        let now = now();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &home.path().join("sessions/2026/07/29/rollout-a.jsonl"),
            &[
                json!({"timestamp": at(now, 60), "type": "session_meta", "payload": {"id": "a"}}),
                codex_turn(&at(now, 60), 100, 10),
                codex_turn(&at(now, 30), 200, 20),
                // Before the window: not this window's spend.
                codex_turn(&at(now, 60 * 30), 5_000, 5_000),
            ],
        );
        // A session that started days ago and is still being appended to.
        write(
            &home.path().join("sessions/2026/07/20/rollout-b.jsonl"),
            &[codex_turn(&at(now, 15), 7, 3)],
        );
        // Nothing countable: not a session.
        write(
            &home.path().join("sessions/2026/07/29/rollout-c.jsonl"),
            &[json!({"timestamp": at(now, 60), "type": "session_meta"})],
        );

        let (spend, unread) = codex_spend(home.path(), now - Duration::hours(6), now);
        assert_eq!(unread, None);
        assert_eq!(spend.input_tokens, 307);
        assert_eq!(spend.output_tokens, 33);
        assert_eq!(spend.total(), 340);
        assert_eq!(spend.sessions, 2);
    }

    #[test]
    fn codex_spend_includes_both_edges_of_its_time_window() {
        let now = now();
        let home = tempfile::TempDir::new().unwrap();
        let since = now - Duration::hours(1);
        write(
            &home.path().join("sessions/2026/07/29/window.jsonl"),
            &[
                codex_turn(&(since - Duration::seconds(1)).to_rfc3339(), 1_000, 1_000),
                codex_turn(&since.to_rfc3339(), 10, 1),
                codex_turn(&now.to_rfc3339(), 20, 2),
                codex_turn(&(now + Duration::seconds(1)).to_rfc3339(), 2_000, 2_000),
            ],
        );

        let (spend, unread) = codex_spend(home.path(), since, now);

        assert_eq!(unread, None);
        assert_eq!(spend.input_tokens, 30);
        assert_eq!(spend.output_tokens, 3);
        assert_eq!(spend.sessions, 1);
    }

    #[test]
    fn claude_spend_takes_one_figure_per_session_and_skips_subagents() {
        let now = now();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &home.path().join("projects/-Users-x-alder/one.jsonl"),
            &[
                claude_turn(&at(now, 60), false, 100, 10),
                // A subagent's own window, which is not the session's.
                claude_turn(&at(now, 40), true, 900_000, 900_000),
                claude_turn(&at(now, 30), false, 300, 30),
            ],
        );
        write(
            &home
                .path()
                .join("projects/-Users-x-alder-work-al-1/two.jsonl"),
            &[claude_turn(&at(now, 20), false, 7, 3)],
        );

        let (spend, unread) = claude_spend(home.path(), now - Duration::hours(6), now);
        assert_eq!(unread, None);
        // The last real assistant entry of each session, and nothing else.
        assert_eq!(spend.input_tokens, 307);
        assert_eq!(spend.output_tokens, 33);
        assert_eq!(spend.sessions, 2);
    }

    #[test]
    fn claude_spend_adds_each_input_usage_component_once() {
        let now = now();
        let home = tempfile::TempDir::new().unwrap();
        let mut turn = claude_turn(&at(now, 10), false, 100, 7);
        let usage = turn
            .pointer_mut("/message/usage")
            .expect("the fixture has usage");
        usage["cache_creation_input_tokens"] = json!(20);
        usage["cache_read_input_tokens"] = json!(3);
        write(
            &home.path().join("projects/-Users-x-alder/one.jsonl"),
            &[turn],
        );

        let (spend, unread) = claude_spend(home.path(), now - Duration::hours(1), now);

        assert_eq!(unread, None);
        assert_eq!(spend.input_tokens, 123);
        assert_eq!(spend.output_tokens, 7);
        assert_eq!(spend.sessions, 1);
    }

    #[test]
    fn turns_outside_the_window_are_not_counted_and_old_files_are_not_opened() {
        let now = now();
        let home = tempfile::TempDir::new().unwrap();
        let project = home.path().join("projects/-Users-x-alder");
        write(
            &project.join("old.jsonl"),
            &[claude_turn(&at(now, 60 * 24 * 7), false, 500, 50)],
        );
        // The file was written just now, so the entry's own timestamp is what
        // keeps a week-old turn out of a six-hour window.
        let (spend, _) = claude_spend(home.path(), now - Duration::hours(6), now);
        assert_eq!(spend.total(), 0);
        assert_eq!(spend.sessions, 0);

        // And a file untouched since before the window is never opened at all.
        assert_eq!(transcripts(&project, now - Duration::hours(6)).len(), 1);
        assert!(transcripts(&project, now + Duration::hours(1)).is_empty());
    }

    #[test]
    fn a_missing_transcript_directory_is_reported_not_guessed_at() {
        let (spend, unread) = codex_spend(Path::new("/nonexistent/codex"), now(), now());
        assert_eq!(spend.total(), 0);
        assert!(unread.unwrap().contains("/nonexistent/codex/sessions"));
        let (spend, unread) = claude_spend(Path::new("/nonexistent/claude"), now(), now());
        assert_eq!(spend.total(), 0);
        assert!(unread.unwrap().contains("/nonexistent/claude/projects"));
    }

    #[test]
    fn a_report_states_the_window_the_basis_and_any_rate_limit() {
        let now = now();
        let mut limits = Limits::default();
        limits.set(
            Provider::Claude,
            now + Duration::hours(1),
            Some("429 mid-turn".to_owned()),
        );
        let report = report(
            now,
            24,
            &limits,
            Path::new("/nonexistent/codex"),
            Path::new("/nonexistent/claude"),
        );
        assert_eq!(report.hours, 24);
        assert_eq!(report.since, now - Duration::hours(24));
        assert_eq!(report.providers.len(), 2);
        assert_eq!(report.providers[0].provider, "codex");
        assert!(report.providers[0].limited_until.is_none());
        assert_eq!(report.providers[1].provider, "claude");
        assert_eq!(
            report.providers[1].limited_until,
            Some(now + Duration::hours(1))
        );
        let text = report.lines().join("\n");
        assert!(text.contains("trailing 24h"), "{text}");
        assert!(text.contains("rate-limited until"), "{text}");
        assert!(text.contains("429 mid-turn"), "{text}");
        assert!(text.contains("real spend in the window"), "{text}");
    }

    #[test]
    fn a_rate_limit_without_a_reason_is_still_rendered() {
        let now = now();
        let mut limits = Limits::default();
        limits.set(Provider::Codex, now + Duration::hours(1), None);
        let report = report(
            now,
            24,
            &limits,
            Path::new("/nonexistent/codex"),
            Path::new("/nonexistent/claude"),
        );

        let text = report.lines().join("\n");

        assert!(text.contains("rate-limited until"), "{text}");
        assert!(!text.contains("rate-limited until  ("), "{text}");
    }
}
