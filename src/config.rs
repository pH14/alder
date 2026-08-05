use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use alder_log::{GitLog, Log};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    domain::{ProjectState, valid_name},
    error::{AlderError, Result},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: String,
    pub prefix: String,
    pub store: StoreConfig,
    pub observers: Vec<ObserverConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    pub remote: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// One configured observer. Exactly one of the two command forms is set:
///
/// - `list` prints a complete JSON snapshot of current levels — the generic
///   contract for non-liveness observers such as CI states;
/// - `probe` answers for one handle at a time. Alder invokes it once per
///   relevant handle with the handle as `$1` and reads back exactly one word:
///   `present`, `absent`, or `unknown`. Execution liveness flows only
///   through probes, so the handle stays opaque to Alder — recognition of a
///   runner's own names lives in the runner's script.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverConfig {
    pub observer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
}

impl ObserverConfig {
    pub fn mode(&self) -> &'static str {
        if self.probe.is_some() {
            "probe"
        } else {
            "list"
        }
    }

    pub fn command(&self) -> &str {
        self.probe
            .as_deref()
            .or(self.list.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct InitResult {
    pub project: Project,
    pub already_initialized: bool,
    pub head_seq: u64,
}

impl Project {
    pub fn discover(start: &Path) -> Result<Self> {
        let mut current = start.canonicalize().map_err(|error| {
            AlderError::with_context(
                "config_missing",
                format!("cannot inspect `{}`: {error}", start.display()),
                json!({"path": start}),
            )
        })?;
        loop {
            let config_path = current.join(".alder/config.json");
            if config_path.is_file() {
                let bytes = fs::read(&config_path)?;
                let config: Config = serde_json::from_slice(&bytes).map_err(|error| {
                    AlderError::with_context(
                        "config_invalid",
                        format!("invalid manifest `{}`: {error}", config_path.display()),
                        json!({"path": config_path}),
                    )
                })?;
                validate_config(&config).map_err(|error| {
                    AlderError::with_context("config_invalid", error.message, error.context)
                })?;
                return Ok(Self {
                    root: current,
                    config_path,
                    config,
                });
            }
            if !current.pop() {
                break;
            }
        }
        Err(AlderError::with_context(
            "config_missing",
            "no .alder/config.json was found",
            json!({"start": start}),
        ))
    }

    pub fn store(&self) -> GitLog {
        GitLog::new(
            &self.root,
            &self.config.store.remote,
            &self.config.store.reference,
        )
        .with_cache(self.store_cache())
    }

    pub fn state_db(&self) -> PathBuf {
        self.root.join(".alder/state.db")
    }

    /// Decoded events of the last read log revision. Like the projection this
    /// is derived local data: deleting it costs one slower read.
    pub fn store_cache(&self) -> PathBuf {
        self.root.join(".alder/cache")
    }

    /// The local append marker. The CLI touches it after every confirmed
    /// append so a co-located driver can stat its mtime instead of waiting
    /// out a full poll interval. It is a hint with zero correctness weight.
    pub fn append_marker(&self) -> PathBuf {
        self.root.join(".alder/last-append")
    }
}

pub fn initialize(start: &Path, prefix: &str, remote: &str, reference: &str) -> Result<InitResult> {
    validate_name("prefix", prefix)?;
    if remote.trim().is_empty() || reference.trim().is_empty() {
        return Err(AlderError::validation(
            "the store remote and ref cannot be empty",
        ));
    }
    let root = git_root(start)?;
    let config_path = root.join(".alder/config.json");
    let existing_bytes = match fs::read(&config_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(bytes) = existing_bytes.as_deref() {
        let config: Config = serde_json::from_slice(bytes).map_err(|error| {
            AlderError::with_context(
                "config_conflict",
                format!("the existing manifest is invalid: {error}"),
                json!({"path": config_path}),
            )
        })?;
        if config.schema != "alder.config.v0"
            || config.prefix != prefix
            || config.store.remote != remote
            || config.store.reference != reference
        {
            return Err(AlderError::with_context(
                "config_conflict",
                "the requested identity or store conflicts with the existing manifest",
                json!({
                    "path": config_path,
                    "requested": {"prefix": prefix, "remote": remote, "ref": reference},
                    "existing": {
                        "schema": config.schema,
                        "prefix": config.prefix,
                        "remote": config.store.remote,
                        "ref": config.store.reference,
                    },
                }),
            ));
        }
        validate_config(&config).map_err(|error| {
            AlderError::with_context("config_conflict", error.message, error.context)
        })?;
        let store = GitLog::new(&root, remote, reference);
        let head = verify_store(&store, prefix)?;
        return Ok(InitResult {
            project: Project {
                root,
                config_path,
                config,
            },
            already_initialized: true,
            head_seq: head,
        });
    }

    let config = Config {
        schema: "alder.config.v0".to_owned(),
        prefix: prefix.to_owned(),
        store: StoreConfig {
            remote: remote.to_owned(),
            reference: reference.to_owned(),
        },
        observers: Vec::new(),
    };
    let store = GitLog::new(&root, remote, reference);
    let head = verify_store(&store, prefix)?;
    let directory = config_path
        .parent()
        .expect("the manifest always has a parent directory");
    fs::create_dir_all(directory)?;
    let mut bytes = serde_json::to_vec_pretty(&config)?;
    bytes.push(b'\n');
    fs::write(&config_path, bytes)?;
    Ok(InitResult {
        project: Project {
            root,
            config_path,
            config,
        },
        already_initialized: false,
        head_seq: head,
    })
}

fn verify_store(store: &GitLog, prefix: &str) -> Result<u64> {
    let head = store.head()?;
    let events = store
        .read_all(&head)?
        .iter()
        .map(crate::domain::decode_record)
        .collect::<Result<Vec<_>>>()?;
    let state = ProjectState::fold(&events).map_err(|error| {
        AlderError::with_context(
            "config_conflict",
            format!(
                "the selected ref is not a compatible Alder log: {}",
                error.message
            ),
            error.context,
        )
    })?;
    state.validate_prefix(prefix)?;
    Ok(head.sequence())
}

fn validate_config(config: &Config) -> Result<()> {
    if config.schema != "alder.config.v0" {
        return Err(AlderError::with_context(
            "config_invalid",
            format!("unsupported config schema `{}`", config.schema),
            json!({"schema": config.schema}),
        ));
    }
    validate_name("prefix", &config.prefix)?;
    if config.store.remote.trim().is_empty() || config.store.reference.trim().is_empty() {
        return Err(AlderError::validation(
            "the store remote and ref cannot be empty",
        ));
    }
    let mut observers = BTreeSet::new();
    for observer in &config.observers {
        validate_name("observer", &observer.observer)?;
        let valid_command = match (&observer.list, &observer.probe) {
            (Some(list), None) => !list.trim().is_empty(),
            (None, Some(probe)) => !probe.trim().is_empty(),
            _ => false,
        };
        if !valid_command {
            return Err(AlderError::validation(format!(
                "observer `{}` must have exactly one non-empty `list` or `probe` command",
                observer.observer
            )));
        }
        if !observers.insert(&observer.observer) {
            return Err(AlderError::validation(format!(
                "observer `{}` is configured more than once",
                observer.observer
            )));
        }
    }
    Ok(())
}

fn validate_name(field: &str, value: &str) -> Result<()> {
    if valid_name(value) {
        Ok(())
    } else {
        Err(AlderError::validation(format!(
            "{field} `{value}` must contain lowercase ASCII letters, digits, or internal hyphens"
        )))
    }
}

fn git_root(start: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()?;
    if !output.status.success() {
        return Err(AlderError::with_context(
            "not_a_git_repository",
            "alder init must run inside a Git repository",
            json!({"path": start}),
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn valid_config() -> Config {
        Config {
            schema: "alder.config.v0".to_owned(),
            prefix: "hm".to_owned(),
            store: StoreConfig {
                remote: "origin".to_owned(),
                reference: "refs/heads/alder".to_owned(),
            },
            observers: vec![ObserverConfig {
                observer: "tmux".to_owned(),
                list: Some("printf '[]'".to_owned()),
                probe: None,
            }],
        }
    }

    #[test]
    fn config_validation_checks_each_identity_and_observer_field() {
        assert!(validate_config(&valid_config()).is_ok());

        let mut config = valid_config();
        config.schema = "alder.config.v1".to_owned();
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.prefix = "Not Valid".to_owned();
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.store.remote = " ".to_owned();
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.store.reference = String::new();
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.observers[0].observer = "Not Valid".to_owned();
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.observers[0].list = Some(" ".to_owned());
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.observers.push(config.observers[0].clone());
        assert!(validate_config(&config).is_err());
    }

    /// An observer runs exactly one command form: `list` for complete generic
    /// snapshots or `probe` for per-handle liveness answers — never both, and
    /// never neither.
    #[test]
    fn an_observer_has_exactly_one_command_form() {
        let mut config = valid_config();
        config.observers[0].list = None;
        config.observers[0].probe = Some("scripts/observe-runner.sh \"$1\"".to_owned());
        assert!(validate_config(&config).is_ok());
        assert_eq!(config.observers[0].mode(), "probe");
        assert_eq!(
            config.observers[0].command(),
            "scripts/observe-runner.sh \"$1\""
        );

        let mut config = valid_config();
        config.observers[0].probe = Some("scripts/observe-runner.sh \"$1\"".to_owned());
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.observers[0].list = None;
        assert!(validate_config(&config).is_err());

        let mut config = valid_config();
        config.observers[0].list = None;
        config.observers[0].probe = Some(" ".to_owned());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn name_validation_accepts_only_the_manifest_name_grammar() {
        for valid in ["a", "hm", "tmux-2", "a1-b2"] {
            assert!(validate_name("field", valid).is_ok(), "{valid}");
        }
        for invalid in ["", "-hm", "hm-", "Upper", "has space", "a_b"] {
            assert!(validate_name("field", invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn discovery_walks_to_the_project_root_and_rejects_invalid_manifests() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("project");
        let child = root.join("one/two");
        fs::create_dir_all(root.join(".alder")).unwrap();
        fs::create_dir_all(&child).unwrap();
        let mut bytes = serde_json::to_vec_pretty(&valid_config()).unwrap();
        bytes.push(b'\n');
        fs::write(root.join(".alder/config.json"), bytes).unwrap();

        let project = Project::discover(&child).unwrap();
        let root = root.canonicalize().unwrap();
        assert_eq!(project.root, root);
        assert_eq!(project.state_db(), root.join(".alder/state.db"));
        // Both derived stores sit beside the manifest, wherever it was found.
        assert_eq!(project.store_cache(), root.join(".alder/cache"));
        assert_eq!(project.config.prefix, "hm");
        assert_eq!(project.config.store.remote, "origin");
        assert_eq!(project.config.store.reference, "refs/heads/alder");

        fs::write(root.join(".alder/config.json"), b"{}").unwrap();
        let error = Project::discover(&child).unwrap_err();
        assert_eq!(error.code, "config_invalid");
    }

    #[test]
    fn initialization_does_not_treat_other_read_errors_as_a_missing_manifest() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("project");
        fs::create_dir_all(root.join(".alder/config.json")).unwrap();
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());

        let error = initialize(&root, "hm", "missing-remote", "refs/heads/alder").unwrap_err();
        assert_eq!(error.code, "io_error");
    }
}
