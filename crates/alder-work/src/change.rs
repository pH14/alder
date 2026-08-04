use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use alder_log::alder_error::{AlderError, Result};

use super::{
    CheckDefinition, NullableString, WorkAppState, WorkDefinition, WorkEventPayload, WorkOperation,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::model::nullable_string_change"
    )]
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
    state: &WorkAppState,
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
            // `edit` never changes state; `work block` and `work unblock` are
            // the verbs that transition it.
            state_change: None,
        });
    }

    let mut candidate = state.clone();
    // The caller supplies the actual sequence during append. Graph validation
    // does not depend on sequence values, so a nonzero placeholder is enough.
    candidate.apply(
        &WorkEventPayload::WorkChanged {
            why: document.why.clone(),
            operations: operations.clone(),
        },
        1,
        "hypothetical",
    )?;

    Ok(PreparedChange {
        operations,
        mappings,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document(value: serde_json::Value) -> GraphChangeDocument {
        serde_json::from_value(value).unwrap()
    }

    fn prepare(value: serde_json::Value, mode: ChangeMode) -> Result<PreparedChange> {
        prepare_change(
            &WorkAppState::default(),
            &document(value),
            mode,
            |index, _| format!("hm-{index}"),
        )
    }

    #[test]
    fn change_modes_enforce_their_distinct_surfaces() {
        assert_eq!(
            prepare(json!({}), ChangeMode::Hypothetical)
                .unwrap_err()
                .message,
            "a graph change must contain at least one operation"
        );
        assert_eq!(
            prepare(
                json!({
                    "why": "edit",
                    "edit": [{"id": "hm-one", "title": "changed"}]
                }),
                ChangeMode::AddOnly,
            )
            .unwrap_err()
            .message,
            "add work --from does not accept an edit section"
        );
        assert_eq!(
            prepare(json!({"add": [{"title": "new"}]}), ChangeMode::Edit)
                .unwrap_err()
                .message,
            "edit work --from requires at least one edit"
        );
        assert_eq!(
            prepare(
                json!({"edit": [{"id": "hm-one", "title": "changed"}]}),
                ChangeMode::Hypothetical,
            )
            .unwrap_err()
            .message,
            "a graph change containing edits requires `why`"
        );
        assert_eq!(
            prepare(
                json!({
                    "why": " ",
                    "edit": [{"id": "hm-one", "title": "changed"}]
                }),
                ChangeMode::Hypothetical,
            )
            .unwrap_err()
            .message,
            "a graph change containing edits requires `why`"
        );
    }

    #[test]
    fn local_names_and_references_are_unambiguous() {
        for local in ["", "$reserved", "has space"] {
            assert!(
                prepare(
                    json!({"add": [{"local": local, "title": "new"}]}),
                    ChangeMode::AddOnly,
                )
                .is_err(),
                "{local}"
            );
        }
        assert!(
            prepare(
                json!({
                    "add": [
                        {"local": "same", "title": "one"},
                        {"local": "same", "title": "two"}
                    ]
                }),
                ChangeMode::AddOnly,
            )
            .is_err()
        );
        assert!(
            prepare(
                json!({
                    "add": [{"title": "new", "requires": ["$missing"]}]
                }),
                ChangeMode::AddOnly,
            )
            .is_err()
        );
    }

    #[test]
    fn successful_changes_resolve_locals_and_reject_ambiguous_edits() {
        let prepared = prepare(
            json!({
                "why": "split",
                "add": [
                    {"local": "first", "title": "first"},
                    {"title": "second", "requires": ["$first"]}
                ]
            }),
            ChangeMode::AddOnly,
        )
        .unwrap();
        assert_eq!(
            prepared.mappings,
            vec![
                ("first".to_owned(), "hm-0".to_owned()),
                ("new-2".to_owned(), "hm-1".to_owned()),
            ]
        );
        match &prepared.operations[1] {
            WorkOperation::Add { work } => {
                assert_eq!(work.id, "hm-1");
                assert_eq!(work.requires, vec!["hm-0"]);
            }
            WorkOperation::Edit { .. } => panic!("expected add"),
        }

        assert!(
            prepare(
                json!({
                    "why": "duplicate",
                    "edit": [
                        {"id": "hm-one", "title": "one"},
                        {"id": "hm-one", "title": "two"}
                    ]
                }),
                ChangeMode::Hypothetical,
            )
            .is_err()
        );
        // `edit` never changes state, so the document has no state fields at all.
        assert!(
            serde_json::from_value::<GraphChangeDocument>(json!({
                "why": "state change",
                "edit": [{"id": "hm-one", "block": true}]
            }))
            .is_err()
        );
    }

    #[test]
    fn change_documents_distinguish_omitted_and_cleared_specs() {
        let omitted = document(json!({
            "why": "leave it",
            "edit": [{"id": "hm-one"}]
        }));
        assert!(omitted.edit[0].spec.is_none());

        let cleared = document(json!({
            "why": "clear it",
            "edit": [{"id": "hm-one", "spec": null}]
        }));
        assert!(matches!(cleared.edit[0].spec, Some(NullableString(None))));
    }
}
