use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    domain::{ProjectState, valid_name},
    error::{AlderError, Result},
    store::{GitStore, Store},
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverConfig {
    pub observer: String,
    pub list: String,
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

    pub fn store(&self) -> GitStore {
        GitStore::new(
            &self.root,
            &self.config.store.remote,
            &self.config.store.reference,
        )
    }

    pub fn state_db(&self) -> PathBuf {
        self.root.join(".alder/state.db")
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
        let store = GitStore::new(&root, remote, reference);
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
    let store = GitStore::new(&root, remote, reference);
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

fn verify_store(store: &GitStore, prefix: &str) -> Result<u64> {
    let head = store.current_head()?;
    let events = store.read_events(&head)?;
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
    Ok(head.seq)
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
        if observer.list.trim().is_empty() {
            return Err(AlderError::validation(format!(
                "observer `{}` has an empty list command",
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
