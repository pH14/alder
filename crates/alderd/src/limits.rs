//! Rate-limit state: one optional timestamp per provider.
//!
//! This is the whole of it. When a spawn or a worker dies on a provider's rate
//! limit, someone records when that provider is expected to be usable again;
//! until then a dispatch aimed at one of its rungs is served by the equivalent
//! rung on the other ladder. After the timestamp passes the entry means
//! nothing and is ignored — no expiry sweep, no daemon, no history.
//!
//! It lives in `.alder/`, which is gitignored, because a rate limit is a fact
//! about one machine's accounts at one moment, not a durable project fact. It
//! is deliberately not in the Alder log for the same reason.

use std::{collections::BTreeMap, fs, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    error::{DriverError, Result},
    tier::Provider,
};

/// Where the state lives, relative to the project root.
pub const LIMITS_FILE: &str = ".alder/rate-limits.json";

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
                return Err(DriverError::new(format!(
                    "cannot read `{}`: {error}",
                    path.display()
                )));
            }
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| DriverError::new(format!("invalid `{}`: {error}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                DriverError::new(format!("cannot create `{}`: {error}", parent.display()))
            })?;
        }
        let mut body = serde_json::to_string_pretty(self)
            .map_err(|error| DriverError::new(format!("cannot serialize rate limits: {error}")))?;
        body.push('\n');
        fs::write(path, body).map_err(|error| {
            DriverError::new(format!("cannot write `{}`: {error}", path.display()))
        })
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
            Some("429 on spawn".to_owned()),
        );
        assert_eq!(
            limits
                .limited(Provider::Codex, now())
                .and_then(|limit| limit.why.clone())
                .as_deref(),
            Some("429 on spawn")
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
}
