//! Raw form edits stay beside the plan. Secrets never enter a tool call, and
//! a receiver change invalidates the secret previously bound to that step.
use super::{settings::SecretDraft, *};
use crate::{
    assistant::agent::Plan,
    tools::{self, FieldKind, FieldSpec, SecretSource, StepId},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use zeroize::Zeroizing;

type Key = (StepId, String);

#[derive(Clone, Default)]
pub(in crate::gui) struct Drafts {
    raw: BTreeMap<Key, String>,
    errors: BTreeMap<Key, String>,
    secrets: BTreeMap<Key, SecretDraft>,
    edited: BTreeSet<Key>,
    touched: BTreeSet<Key>,
    just_edited: Option<Key>,
    changed: BTreeMap<Key, String>,
}

impl fmt::Debug for Drafts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Drafts")
            .field("fields", &self.raw.len())
            .field("secrets", &"[redacted]")
            .finish()
    }
}

struct Secrets(BTreeMap<Key, SecretDraft>);
impl SecretSource for Secrets {
    fn secret(&self, step: StepId, field: &str) -> Option<Zeroizing<String>> {
        self.0
            .get(&(step, field.into()))
            .map(|value| Zeroizing::new(value.as_str().to_owned()))
    }
}

impl Drafts {
    pub(in crate::gui) fn text(&self, step: StepId, key: &str, args: &Value) -> String {
        self.raw
            .get(&(step, key.into()))
            .cloned()
            .unwrap_or_else(|| {
                if key == "access_key_id" {
                    at(args, key).as_str().unwrap_or("").to_owned()
                } else if key.starts_with("region.") && args["region"].is_null() {
                    if matches!(key, "region.x" | "region.y") {
                        "0"
                    } else {
                        "1"
                    }
                    .into()
                } else {
                    display(at(args, key))
                }
            })
    }

    pub(in crate::gui) fn secret_text(&self, step: StepId, key: &str) -> &str {
        self.secrets
            .get(&(step, key.into()))
            .map_or("", SecretDraft::as_str)
    }

    pub(in crate::gui) fn issue(&self, step: StepId, key: &str) -> Option<&str> {
        self.errors.get(&(step, key.into())).map(String::as_str)
    }

    pub(in crate::gui) fn just_edited(&self, step: StepId, key: &str) -> bool {
        self.just_edited
            .as_ref()
            .is_some_and(|(id, field)| *id == step && field == key)
    }

    pub(in crate::gui) fn visible(&self, step: StepId, key: &str, args: &Value) -> bool {
        !self.text(step, key, args).is_empty() || self.touched.contains(&(step, key.into()))
    }

    pub(in crate::gui) fn has_draft(&self, step: StepId, key: &str) -> bool {
        self.raw.keys().any(|(id, field)| {
            *id == step && (field == key || field.starts_with(&format!("{key}.")))
        })
    }

    pub(in crate::gui) fn changed_fields(&self, step: StepId) -> Vec<String> {
        self.changed
            .iter()
            .filter(|((id, _), _)| *id == step)
            .map(|(_, label)| label.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(in crate::gui) fn changed(&self, step: StepId, key: &str) -> bool {
        self.changed.contains_key(&(step, key.into()))
    }

    pub(in crate::gui) fn snapshot(&self) -> Arc<dyn SecretSource + Send + Sync> {
        Arc::new(Secrets(
            self.secrets
                .iter()
                .filter(|(key, _)| !self.errors.contains_key(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ))
    }

    pub(in crate::gui) fn clear_secrets(&mut self) {
        self.secrets.clear();
        self.just_edited = None;
        self.errors
            .retain(|(_, key), _| !matches!(key.as_str(), "secret" | "password"));
    }

    pub(in crate::gui) fn secret(&mut self, step: StepId, key: &str, value: SecretDraft) {
        let key = (step, key.to_owned());
        self.just_edited = Some(key.clone());
        self.touched.insert(key.clone());
        self.errors.remove(&key);
        if value.as_str().len() > 4096 {
            self.errors
                .insert(key.clone(), "Enter a secret of at most 4,096 bytes.".into());
        }
        if value.as_str().is_empty() {
            self.secrets.remove(&key);
        } else {
            self.secrets.insert(key, value);
        }
    }

    pub(in crate::gui) fn edit(
        &mut self,
        plan: &mut Plan,
        step: StepId,
        key: &str,
        value: String,
    ) -> Result<(), String> {
        let previous = plan
            .steps
            .iter()
            .find(|item| item.id == step)
            .ok_or("Choose an existing plan step.")?;
        let spec = field(previous, key).ok_or("Choose an existing step field.")?;
        if spec.ui_only {
            return Err("Enter secrets only in their secure fields.".into());
        }
        if !key.contains('.')
            && matches!(
                spec.kind,
                FieldKind::Features | FieldKind::Tags | FieldKind::StringList | FieldKind::Region
            )
        {
            let prefix = format!("{key}.");
            self.raw
                .retain(|(id, key), _| *id != step || !key.starts_with(&prefix));
            self.errors
                .retain(|(id, key), _| *id != step || !key.starts_with(&prefix));
            self.edited
                .retain(|(id, key)| *id != step || !key.starts_with(&prefix));
            self.touched
                .retain(|(id, key)| *id != step || !key.starts_with(&prefix));
            self.changed
                .retain(|(id, key), _| *id != step || !key.starts_with(&prefix));
        }
        let mut call = previous.call.clone();
        if spec.kind == FieldKind::Region && key.contains('.') && call.args[spec.key].is_null() {
            call.args[spec.key] = json!({"x":0.0,"y":0.0,"width":1.0,"height":1.0});
        }
        let user_camera = previous.user_entered_camera || key == "camera";
        let parsed = parse(spec, key, &value);
        let entry = (step, key.to_owned());
        match parsed {
            Ok(value) => {
                self.errors.remove(&entry);
                put(&mut call.args, key, value, spec.kind)?;
            }
            Err(error) => {
                self.errors.insert(entry.clone(), error);
                put(&mut call.args, key, Value::String(value.clone()), spec.kind)?;
            }
        }
        rewrite_extension(&mut call.args, &previous.call.args, key);
        if call.args["file_name"] != previous.call.args["file_name"] {
            self.raw.remove(&(step, "file_name".into()));
            self.errors.remove(&(step, "file_name".into()));
        }
        if call.args != previous.call.args {
            self.invalidate_secrets(step);
        }
        self.raw.insert(entry.clone(), value);
        self.edited.insert(entry.clone());
        self.just_edited = Some(entry.clone());
        self.changed.remove(&entry);
        plan.edit(step, call, user_camera)
            .map_err(|error| error.to_string())
    }

    pub(in crate::gui) fn set_value(
        &mut self,
        plan: &mut Plan,
        step: StepId,
        key: &str,
        value: Value,
    ) -> Result<(), String> {
        let raw = match value {
            Value::Null => String::new(),
            Value::String(value) => value,
            value => value.to_string(),
        };
        self.edit(plan, step, key, raw)
    }

    pub(in crate::gui) fn commit(
        &mut self,
        plan: &mut Plan,
        step: StepId,
        key: &str,
    ) -> Result<(), String> {
        if !plan.steps.iter().any(|item| item.id == step) {
            return Err("Choose an existing plan step.".into());
        }
        let entry = (step, key.to_owned());
        self.touched.insert(entry.clone());
        self.just_edited = Some(entry.clone());
        if !self.errors.contains_key(&entry) {
            self.raw.remove(&entry);
        }
        Ok(())
    }

    pub(in crate::gui) fn recheck(
        &self,
        plan: &mut Plan,
        context: &tools::context::Context,
        actor: Option<String>,
    ) {
        let secrets = self.snapshot();
        plan.recheck(context, actor, secrets.as_ref());
        for step in &mut plan.steps {
            if let Some(checked) = &mut step.checked {
                // The form only offers cached camera choices. A previously
                // explicit selector must not silently turn into an unknown
                // address after a refresh; the CLI still permits typed ones.
                if let Some(spec) = tools::spec(&step.call.tool) {
                    for field in spec
                        .fields
                        .iter()
                        .filter(|field| field.kind == FieldKind::Camera)
                    {
                        if let Some(selector) = step.call.args[field.key]
                            .as_str()
                            .filter(|selector| !selector.is_empty())
                            && tools::resolve_camera(context, field.key, selector, true).is_err()
                            && !checked.issues.iter().any(|issue| {
                                issue.is_error() && issue.field.as_deref() == Some(field.key)
                            })
                        {
                            checked.issue(tools::Issue::error(
                                Some(field.key),
                                "Choose an available camera from the list.",
                                tools::IssueCode::Unavailable,
                            ));
                        }
                    }
                }
                for ((id, key), message) in &self.errors {
                    if *id == step.id {
                        checked
                            .issues
                            .retain(|issue| issue.field.as_deref() != Some(key));
                        checked.issue(tools::Issue::usage(Some(key), message));
                    }
                }
            }
        }
    }

    /// A replacement can retain a StepId while changing the recipient. Never
    /// carry its previous secret or invalid raw input into that new call.
    pub(in crate::gui) fn reconcile(&mut self, before: &Plan, after: &Plan) {
        let retained: BTreeSet<_> = after.steps.iter().map(|step| step.id).collect();
        self.raw.retain(|(id, _), _| retained.contains(id));
        self.errors.retain(|(id, _), _| retained.contains(id));
        self.secrets.retain(|(id, _), _| retained.contains(id));
        self.edited.retain(|(id, _)| retained.contains(id));
        self.touched.retain(|(id, _)| retained.contains(id));
        self.changed.retain(|(id, _), _| retained.contains(id));
        for next in &after.steps {
            let Some(previous) = before.steps.iter().find(|step| step.id == next.id) else {
                continue;
            };
            if previous.call == next.call {
                continue;
            }
            self.invalidate_secrets(next.id);
            if self
                .just_edited
                .as_ref()
                .is_some_and(|(id, _)| *id == next.id)
            {
                self.just_edited = None;
            }
            let keys: Vec<_> = self
                .edited
                .iter()
                .filter(|(id, _)| *id == next.id)
                .cloned()
                .collect();
            for key in keys {
                if previous.call.tool != next.call.tool
                    || at(&previous.call.args, &key.1) != at(&next.call.args, &key.1)
                {
                    if next.changed_by_assistant {
                        let label = field(next, &key.1).map_or(key.1.as_str(), |field| field.label);
                        self.changed.insert(key.clone(), label.into());
                    }
                    self.raw.remove(&key);
                    self.errors.remove(&key);
                }
            }
        }
    }

    fn invalidate_secrets(&mut self, step: StepId) {
        self.secrets.retain(|(id, _), _| *id != step);
        self.errors
            .retain(|(id, key), _| *id != step || !matches!(key.as_str(), "secret" | "password"));
    }
}

fn field(step: &crate::assistant::agent::PlanStep, key: &str) -> Option<&'static FieldSpec> {
    let root = key.split('.').next()?;
    tools::spec(&step.call.tool)?
        .fields
        .iter()
        .find(|field| field.key == root)
}

pub(in crate::gui) fn at<'a>(args: &'a Value, key: &str) -> &'a Value {
    key.split('.')
        .fold(args, |value, part| match part.parse::<usize>() {
            Ok(index) => &value[index],
            Err(_) => &value[part],
        })
}

fn put(args: &mut Value, key: &str, value: Value, kind: FieldKind) -> Result<(), String> {
    if kind == FieldKind::StringList
        && let Some((root, index)) = key.split_once('.')
    {
        let entry = index
            .parse::<usize>()
            .ok()
            .and_then(|index| args[root].as_array_mut()?.get_mut(index))
            .ok_or("Choose an existing list row.")?;
        *entry = value;
        return Ok(());
    }
    let parts: Vec<_> = key.split('.').collect();
    let mut current = args;
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        if let Ok(offset) = part.parse::<usize>() {
            if offset >= 16 {
                return Err("Keep lists to at most 16 entries.".into());
            }
            if !current.is_array() {
                *current = json!([]);
            }
            let array = current.as_array_mut().unwrap();
            array.resize(array.len().max(offset + 1), Value::Null);
            if last {
                array[offset] = value;
                return Ok(());
            }
            current = &mut array[offset];
        } else {
            if !current.is_object() {
                *current = json!({});
            }
            if last {
                current[*part] = value;
                return Ok(());
            }
            current = &mut current[*part];
        }
    }
    Ok(())
}

fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => tools::redact::for_output_text(value),
        value => tools::redact::for_output(value.clone()).to_string(),
    }
}

fn parse(spec: &FieldSpec, key: &str, raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if key.contains('.') {
        return match spec.kind {
            FieldKind::Region => trimmed
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                .map(|value| json!(value))
                .ok_or_else(|| "Enter a number from 0 to 1.".into()),
            FieldKind::Features if key.ends_with(".expect") && trimmed.is_empty() => {
                Ok(Value::Null)
            }
            FieldKind::Features | FieldKind::Tags => Ok(Value::String(raw.into())),
            FieldKind::StringList
                if key
                    .split_once('.')
                    .is_some_and(|(_, index)| index.parse::<usize>().is_ok()) =>
            {
                Ok(Value::String(raw.into()))
            }
            _ => Err("Choose an existing step field.".into()),
        };
    }
    if trimmed.is_empty() && spec.optional {
        return Ok(Value::Null);
    }
    match spec.kind {
        FieldKind::Integer { min, max } => trimmed
            .parse::<i64>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .map(|value| json!(value))
            .ok_or_else(|| format!("Enter a whole number from {min} to {max}.")),
        FieldKind::Number { min, max } => trimmed
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && (min..=max).contains(value))
            .map(|value| json!(value))
            .ok_or_else(|| format!("Enter a number from {min} to {max}.")),
        FieldKind::Duration => tools::parse_duration_field(trimmed).map(|_| json!(trimmed)),
        FieldKind::Bool => trimmed
            .parse::<bool>()
            .map(|value| json!(value))
            .map_err(|_| "Choose on or off.".into()),
        FieldKind::Choice(choices) => choices
            .iter()
            .any(|(value, _)| *value == trimmed)
            .then(|| json!(trimmed))
            .ok_or_else(|| "Choose an option from the list.".into()),
        FieldKind::Features | FieldKind::Tags => tools::plan::parse_value(raw)
            .ok()
            .filter(Value::is_array)
            .ok_or_else(|| "Enter a list of values.".into()),
        FieldKind::StringList => tools::plan::parse_value(raw)
            .ok()
            .filter(|value| {
                value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            })
            .ok_or_else(|| "Enter a list of names.".into()),
        FieldKind::Region => tools::plan::parse_value(raw)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| "Enter left, top, width and height from 0 to 1.".into()),
        FieldKind::Secret => Err("Enter secrets only in their secure fields.".into()),
        _ => Ok(Value::String(raw.into())),
    }
}

fn rewrite_extension(args: &mut Value, previous: &Value, key: &str) {
    let Some(name) = previous["file_name"].as_str() else {
        return;
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return;
    };
    let replacement = if key == "format" {
        let old = previous["format"].as_str().unwrap_or("png");
        let new = args["format"].as_str().unwrap_or("png");
        (extension.eq_ignore_ascii_case(old)
            || old == "jpeg" && extension.eq_ignore_ascii_case("jpg"))
        .then_some(new)
    } else if key == "codec"
        && extension.eq_ignore_ascii_case("webm")
        && args["codec"].as_str() != Some("av1")
    {
        Some("mp4")
    } else {
        None
    };
    if let Some(extension) = replacement {
        args["file_name"] = json!(format!("{stem}.{extension}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> (Plan, StepId) {
        let mut plan = Plan::default();
        let id = plan.propose(tools::ToolCall { tool: "capture_photos".into(), args: json!({"camera":"sim:0","count":1,"format":"png","file_name":"photo.png","destination":null,"tags":null}) }, None).unwrap();
        (plan, id)
    }

    #[test]
    fn invalid_numbers_keep_raw_text_and_issue_until_fixed() {
        let (mut plan, id) = plan();
        let mut drafts = Drafts::default();
        drafts.edit(&mut plan, id, "count", "ten".into()).unwrap();
        assert_eq!(plan.steps[0].call.args["count"], "ten");
        assert!(drafts.issue(id, "count").unwrap().contains("whole number"));
        drafts.commit(&mut plan, id, "count").unwrap();
        assert_eq!(drafts.text(id, "count", &plan.steps[0].call.args), "ten");
        drafts.edit(&mut plan, id, "count", "4".into()).unwrap();
        assert!(drafts.issue(id, "count").is_none());
        assert_eq!(plan.steps[0].call.args["count"], 4);
    }

    #[test]
    fn receiver_replacement_invalidates_secrets_even_when_the_id_is_stable() {
        let (mut plan, id) = plan();
        let mut drafts = Drafts::default();
        drafts.secret(id, "password", "hidden-password".to_owned().into());
        assert!(drafts.snapshot().secret(id, "password").is_some());
        let before = plan.clone();
        let mut call = plan.steps[0].call.clone();
        call.args["destination"] = json!("different");
        plan.propose(call, Some(id)).unwrap();
        drafts.reconcile(&before, &plan);
        assert!(drafts.snapshot().secret(id, "password").is_none());
        assert!(!format!("{drafts:?}").contains("hidden-password"));
        assert!(
            !plan.document().calls[0]
                .args
                .to_string()
                .contains("hidden-password")
        );
    }

    #[test]
    fn overlong_secrets_remain_private_and_cannot_be_used_by_a_worker() {
        let mut drafts = Drafts::default();
        let id = StepId(1);
        drafts.secret(id, "password", "x".repeat(4097).into());
        assert!(drafts.issue(id, "password").unwrap().contains("4,096"));
        assert!(drafts.snapshot().secret(id, "password").is_none());
        drafts.secret(id, "password", "valid secret é".to_owned().into());
        assert!(drafts.issue(id, "password").is_none());
        assert!(drafts.snapshot().secret(id, "password").is_some());
        drafts.clear_secrets();
        assert!(drafts.snapshot().secret(id, "password").is_none());
        assert!(drafts.secret_text(id, "password").is_empty());
    }

    #[test]
    fn nested_fields_keep_json_types_and_format_changes_rewrite_matching_extensions() {
        let (mut plan, id) = plan();
        let mut drafts = Drafts::default();
        drafts
            .set_value(&mut plan, id, "format", json!("jpeg"))
            .unwrap();
        assert_eq!(plan.steps[0].call.args["file_name"], "photo.jpeg");
        drafts
            .edit(&mut plan, id, "tags.0.key", "part".into())
            .unwrap();
        drafts
            .edit(&mut plan, id, "tags.0.value", "part-a".into())
            .unwrap();
        assert_eq!(
            plan.steps[0].call.args["tags"],
            json!([{"key":"part","value":"part-a"}])
        );
    }

    #[test]
    fn replacing_a_list_discards_indexed_drafts_instead_of_moving_them_to_another_row() {
        let (mut plan, id) = plan();
        let mut drafts = Drafts::default();
        drafts
            .edit(&mut plan, id, "tags.0.key", "first".into())
            .unwrap();
        drafts
            .edit(&mut plan, id, "tags.0.value", "original".into())
            .unwrap();
        drafts
            .set_value(
                &mut plan,
                id,
                "tags",
                json!([{"key":"second","value":"replacement"}]),
            )
            .unwrap();
        assert_eq!(
            drafts.text(id, "tags.0.key", &plan.steps[0].call.args),
            "second"
        );
        assert_eq!(
            drafts.text(id, "tags.0.value", &plan.steps[0].call.args),
            "replacement"
        );
    }

    #[test]
    fn editing_a_whole_frame_region_supplies_the_other_three_defaults() {
        let mut plan = Plan::default();
        let id = plan.propose(tools::ToolCall {tool:"inspect_frame".into(),args:json!({"camera":"sim:0","fresh":null,"max_dimension":null,"region":null,"include_image":null})},None).unwrap();
        let mut drafts = Drafts::default();
        drafts
            .edit(&mut plan, id, "region.width", "0.5".into())
            .unwrap();
        assert_eq!(
            plan.steps[0].call.args["region"],
            json!({"x":0.0,"y":0.0,"width":0.5,"height":1.0})
        );
        drafts
            .set_value(&mut plan, id, "region", Value::Null)
            .unwrap();
        assert_eq!(
            drafts.text(id, "region.width", &plan.steps[0].call.args),
            "1"
        );
    }

    #[test]
    fn replacing_an_edited_field_marks_it_and_discards_its_previous_raw_draft() {
        let (mut plan, id) = plan();
        let mut drafts = Drafts::default();
        drafts.edit(&mut plan, id, "count", "4".into()).unwrap();
        let before = plan.clone();
        let mut call = plan.steps[0].call.clone();
        call.args["count"] = json!(8);
        plan.propose(call, Some(id)).unwrap();
        drafts.reconcile(&before, &plan);
        assert_eq!(drafts.text(id, "count", &plan.steps[0].call.args), "8");
        assert_eq!(drafts.changed_fields(id), ["Photos"]);
    }

    #[test]
    fn a_stale_explicit_camera_choice_blocks_the_gui_without_restricting_cli_selectors() {
        let mut context = super::super::scenes::context(SessionSnapshot::default());
        context.snapshot = SessionSnapshot::default();
        let mut plan = Plan::default();
        let call = tools::ToolCall {
            tool: "connect_camera".into(),
            args: json!({"camera":"sim:99","size":null,"fps":null}),
        };
        let id = plan.propose(call.clone(), None).unwrap();
        plan.edit(id, call, true).unwrap();
        let cli = tools::check_plan(&context, &plan.calls(), &|_, _| false);
        assert!(
            cli[0].ready(),
            "the CLI retains explicit simulator selectors"
        );
        Drafts::default().recheck(&mut plan, &context, None);
        let checked = plan.steps[0].checked.as_ref().unwrap();
        assert!(!checked.ready());
        assert!(
            checked
                .issues
                .iter()
                .any(|issue| issue.field.as_deref() == Some("camera")
                    && issue.code == tools::IssueCode::Unavailable)
        );
    }

    #[test]
    fn a_known_camera_name_is_a_valid_form_choice() {
        let context = super::super::scenes::context(SessionSnapshot::default());
        let name = context.snapshot.cameras[0].info.model.clone();
        let mut plan = Plan::default();
        plan.propose(
            tools::ToolCall {
                tool: "connect_camera".into(),
                args: json!({"camera":name,"size":null,"fps":null}),
            },
            None,
        )
        .unwrap();
        Drafts::default().recheck(&mut plan, &context, None);
        assert!(plan.steps[0].checked.as_ref().unwrap().ready());
    }
}
