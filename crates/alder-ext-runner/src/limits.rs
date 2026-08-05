//! Rate-limit state: one optional timestamp per provider.
//!
//! This is the whole of it. When a start or an execution dies on a provider's
//! rate limit, someone records when that provider is expected to be usable
//! again; until then a start aimed at one of its rungs is served by the
//! equivalent rung on the other ladder. After the timestamp passes the entry
//! means nothing and is ignored — no expiry sweep, no daemon, no history.
//!
//! It lives in the runner's machine-local state directory (see
//! [`crate::config::limits_path`]) because a rate limit is a fact about one
//! machine's accounts at one moment, not a fact about any repository.
//!
//! **Fail-open, deliberately.** Limits are hygiene, not authority: they only
//! reroute a start to the other ladder, and the worst a lost or corrupt file
//! costs is one start against a limited provider. So a corrupt file is
//! complained about loudly, treated as empty, and rewritten whole — never a
//! reason to refuse a start. What is guarded is the mechanics: every
//! read-modify-write runs under an exclusive file lock, and every write goes
//! through a temporary file and an atomic rename, so two concurrent `limit`
//! commands cannot shred the file or silently drop each other's entries.

use std::{collections::BTreeMap, fs, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    error::{Result, RunnerError},
    tier::Provider,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limit {
    /// When the provider is expected to be usable again.
    pub until: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub providers: BTreeMap<String, Limit>,
}

impl Limits {
    /// Read the state. A missing file is an empty state, not an error: no file
    /// simply means nothing has been rate-limited on this machine.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(RunnerError::new(format!(
                    "cannot read `{}`: {error}",
                    path.display()
                )));
            }
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| RunnerError::new(format!("invalid `{}`: {error}", path.display())))
    }

    /// Write the state atomically: a temporary file beside the target, then
    /// one rename. A reader never sees a half-written document, whatever
    /// happens to this process mid-write.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RunnerError::new(format!("cannot create `{}`: {error}", parent.display()))
            })?;
        }
        let mut body = serde_json::to_string_pretty(self)
            .map_err(|error| RunnerError::new(format!("cannot serialize rate limits: {error}")))?;
        body.push('\n');
        let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&temporary, body).map_err(|error| {
            RunnerError::new(format!("cannot write `{}`: {error}", temporary.display()))
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            RunnerError::new(format!(
                "cannot move `{}` into place at `{}`: {error}",
                temporary.display(),
                path.display()
            ))
        })
    }

    /// One locked read-modify-write of the state file.
    ///
    /// The exclusive lock covers the whole load-mutate-save, so concurrent
    /// updates serialize instead of losing each other's entries. A corrupt
    /// file fails open: it is complained about loudly, treated as empty, and
    /// rewritten atomically — limits are hygiene, not authority.
    pub fn update(path: &Path, mutate: impl FnOnce(&mut Self)) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RunnerError::new(format!("cannot create `{}`: {error}", parent.display()))
            })?;
        }
        let lock_path = path.with_extension("lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                RunnerError::new(format!("cannot open `{}`: {error}", lock_path.display()))
            })?;
        // Blocking, unlike the per-handle start lock: the critical section is
        // one small file rewrite, and a `limit` command that waited a moment
        // is better than one that made the operator retype it.
        lock.lock().map_err(|error| {
            RunnerError::new(format!("cannot lock `{}`: {error}", lock_path.display()))
        })?;
        let mut limits = match Self::load(path) {
            Ok(limits) => limits,
            Err(error) => {
                eprintln!(
                    "alder-ext-runner: the rate-limit state is unreadable and will be \
                     rewritten from empty (limits are hygiene, not authority): {error}"
                );
                Self::default()
            }
        };
        mutate(&mut limits);
        limits.save(path)?;
        Ok(limits)
    }

    /// The live limit on a provider, or `None` once it has expired.
    pub fn limited(&self, provider: Provider, now: DateTime<Utc>) -> Option<&Limit> {
        self.providers
            .get(provider.as_str())
            .filter(|limit| limit.until > now)
    }

    pub fn set(&mut self, provider: Provider, until: DateTime<Utc>, why: Option<String>) {
        self.providers
            .insert(provider.as_str().to_owned(), Limit { until, why });
    }

    pub fn clear(&mut self, provider: Provider) {
        self.providers.remove(provider.as_str());
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-07-29T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_state() {
        let limits = Limits::load(Path::new("/nonexistent/rate-limits.json")).unwrap();
        assert!(limits.providers.is_empty());
        assert!(limits.limited(Provider::Codex, now()).is_none());
    }

    #[test]
    fn an_error_other_than_a_missing_file_is_not_an_empty_state() {
        let directory = tempfile::TempDir::new().unwrap();
        let error = Limits::load(directory.path()).expect_err("a directory is not a limits file");
        assert!(error.message.contains("cannot read"), "{error}");
    }

    #[test]
    fn a_limit_expires_on_its_own_without_being_swept() {
        let mut limits = Limits::default();
        limits.set(
            Provider::Codex,
            now() + Duration::minutes(30),
            Some("429 on start".to_owned()),
        );
        assert_eq!(
            limits
                .limited(Provider::Codex, now())
                .and_then(|limit| limit.why.clone())
                .as_deref(),
            Some("429 on start")
        );
        assert!(limits.limited(Provider::Claude, now()).is_none());
        // Past its timestamp the entry is still on disk and means nothing.
        assert!(
            limits
                .limited(Provider::Codex, now() + Duration::hours(1))
                .is_none()
        );
        assert!(limits.providers.contains_key("codex"));
        limits.clear(Provider::Codex);
        assert!(limits.providers.is_empty());
    }

    #[test]
    fn a_limit_at_its_exact_deadline_is_expired() {
        let mut limits = Limits::default();
        limits.set(Provider::Codex, now(), None);
        assert!(limits.limited(Provider::Codex, now()).is_none());
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("nested/rate-limits.json");
        let mut limits = Limits::default();
        limits.set(Provider::Claude, now() + Duration::hours(2), None);
        limits.save(&path).unwrap();
        let read = Limits::load(&path).unwrap();
        assert_eq!(
            read.limited(Provider::Claude, now())
                .map(|limit| limit.until),
            Some(now() + Duration::hours(2))
        );
        assert!(read.limited(Provider::Claude, now()).unwrap().why.is_none());

        fs::write(&path, "not json").unwrap();
        assert!(Limits::load(&path).is_err());
    }

    #[test]
    fn a_save_leaves_no_temporary_beside_the_state_file() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("rate-limits.json");
        let mut limits = Limits::default();
        limits.set(Provider::Codex, now(), None);
        limits.save(&path).unwrap();

        let names: Vec<String> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["rate-limits.json"], "{names:?}");
    }

    #[test]
    fn an_update_over_a_corrupt_file_fails_open_and_rewrites_it_whole() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("rate-limits.json");
        fs::write(&path, "not json").unwrap();

        let updated = Limits::update(&path, |limits| {
            limits.set(Provider::Claude, now() + Duration::hours(1), None);
        })
        .expect("a corrupt limits file is hygiene, not a refusal");
        assert!(updated.limited(Provider::Claude, now()).is_some());

        // The file is valid again, holding exactly the update.
        let read = Limits::load(&path).expect("the rewritten file is valid");
        assert!(read.limited(Provider::Claude, now()).is_some());
        assert!(read.limited(Provider::Codex, now()).is_none());
    }

    #[test]
    fn concurrent_updates_serialize_and_lose_no_entries() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("rate-limits.json");

        std::thread::scope(|scope| {
            for provider in Provider::ALL {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..25 {
                        Limits::update(path, |limits| {
                            limits.set(provider, now() + Duration::hours(1), None);
                        })
                        .expect("a locked update succeeds");
                    }
                });
            }
        });

        // Interleaved load-modify-save without the lock would let one side's
        // last write drop the other's entry.
        let read = Limits::load(&path).expect("the contested file is intact");
        for provider in Provider::ALL {
            assert!(
                read.limited(provider, now()).is_some(),
                "{} was lost to a concurrent update",
                provider.as_str()
            );
        }
    }
}
