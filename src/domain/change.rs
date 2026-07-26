use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::error::{AlderError, Result};

use super::{
    CheckDefinition, NullableString, ProjectState, WorkDefinition, WorkOperation, WorkStateChange,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphChangeDocument {
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub add: Vec<AddWorkInput>,
    #[serde(default)]
    pub edit: Vec<EditWorkInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddWorkInput {
    #[serde(default)]
    pub local: Option<String>,
    pub title: String,
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub checks: Vec<CheckDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditWorkInput {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub spec: Option<NullableString>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub add_requires: Vec<String>,
    #[serde(default)]
    pub remove_requires: Vec<String>,
    #[serde(default)]
    pub add_checks: Vec<CheckDefinition>,
    #[serde(default)]
    pub remove_checks: Vec<String>,
    #[serde(default)]
    pub block: bool,
    #[serde(default)]
    pub unblock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeMode {
    AddOnly,
    Edit,
    Hypothetical,
}

#[derive(Debug, Clone)]
pub struct PreparedChange {
    pub operations: Vec<WorkOperation>,
    pub mappings: Vec<(String, String)>,
}

pub fn prepare_change<F>(
    state: &ProjectState,
    document: &GraphChangeDocument,
    mode: ChangeMode,
    mut allocate: F,
) -> Result<PreparedChange>
where
    F: FnMut(usize, Option<&str>) -> String,
{
    if document.add.is_empty() && document.edit.is_empty() {
        return Err(AlderError::validation(
            "a graph change must contain at least one operation",
        ));
    }
    match mode {
        ChangeMode::AddOnly if !document.edit.is_empty() => {
            return Err(AlderError::validation(
                "add work --from does not accept an edit section",
            ));
        }
        ChangeMode::Edit if document.edit.is_empty() => {
            return Err(AlderError::validation(
                "edit work --from requires at least one edit",
            ));
        }
        _ => {}
    }
    if !document.edit.is_empty()
        && document
            .why
            .as_deref()
            .is_none_or(|why| why.trim().is_empty())
    {
        return Err(AlderError::validation(
            "a graph change containing edits requires `why`",
        ));
    }

    let mut local_ids = BTreeMap::new();
    let mut mappings = Vec::new();
    for (index, add) in document.add.iter().enumerate() {
        let display = add
            .local
            .clone()
            .unwrap_or_else(|| format!("new-{}", index + 1));
        if display.is_empty()
            || display.starts_with('$')
            || display.chars().any(char::is_whitespace)
        {
            return Err(AlderError::validation(format!(
                "invalid local work name `{display}`"
            )));
        }
        let key = format!("${display}");
        if local_ids.contains_key(&key) {
            return Err(AlderError::validation(format!(
                "duplicate local work name `{display}`"
            )));
        }
        let id = allocate(index, add.local.as_deref());
        local_ids.insert(key, id.clone());
        mappings.push((display, id));
    }

    let resolve = |value: &str| -> Result<String> {
        if value.starts_with('$') {
            local_ids
                .get(value)
                .cloned()
                .ok_or_else(|| AlderError::validation(format!("unknown local reference `{value}`")))
        } else {
            Ok(value.to_owned())
        }
    };

    let mut operations = Vec::new();
    for (index, add) in document.add.iter().enumerate() {
        let id = mappings[index].1.clone();
        operations.push(WorkOperation::Add {
            work: WorkDefinition {
                id,
                title: add.title.clone(),
                spec: add.spec.clone(),
                priority: add.priority,
                requires: add
                    .requires
                    .iter()
                    .map(|required| resolve(required))
                    .collect::<Result<_>>()?,
                checks: add.checks.clone(),
            },
        });
    }
    let mut edit_targets = BTreeSet::new();
    for edit in &document.edit {
        let id = resolve(&edit.id)?;
        if !edit_targets.insert(id.clone()) {
            return Err(AlderError::validation(format!(
                "work `{id}` is targeted more than once"
            )));
        }
        if edit.block && edit.unblock {
            return Err(AlderError::validation(format!(
                "work `{id}` cannot be blocked and unblocked in one operation"
            )));
        }
        let reason = document.why.clone().unwrap_or_default();
        let state_change = if edit.block {
            Some(WorkStateChange::Block {
                reason: reason.clone(),
            })
        } else if edit.unblock {
            Some(WorkStateChange::Unblock { reason })
        } else {
            None
        };
        operations.push(WorkOperation::Edit {
            id,
            title: edit.title.clone(),
            spec: edit.spec.clone(),
            priority: edit.priority,
            add_requires: edit
                .add_requires
                .iter()
                .map(|required| resolve(required))
                .collect::<Result<_>>()?,
            remove_requires: edit
                .remove_requires
                .iter()
                .map(|required| resolve(required))
                .collect::<Result<_>>()?,
            add_checks: edit.add_checks.clone(),
            remove_checks: edit.remove_checks.clone(),
            state_change,
        });
    }

    let mut candidate = state.clone();
    let event = super::Event {
        id: "hypothetical".to_owned(),
        seq: 1,
        at: chrono::Utc::now(),
        actor: "hypothetical".to_owned(),
        payload: super::EventPayload::WorkChanged {
            why: document.why.clone(),
            operations: operations.clone(),
        },
        schema: "alder.event.v0".to_owned(),
    };
    // The caller supplies the actual sequence during append. Graph validation
    // does not depend on sequence values, so a nonzero placeholder is enough.
    candidate.apply(&event)?;

    Ok(PreparedChange {
        operations,
        mappings,
    })
}
