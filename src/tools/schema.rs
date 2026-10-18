//! Tool argument schemas in a portable subset of JSON Schema that every
//! provider's tool calling accepts, strict modes included: closed objects
//! whose keys are all required, nullable types for optional values, and only
//! the keywords type, properties, required, additionalProperties, items,
//! enum and description. Ranges, lengths and formats are checked in Rust
//! and stated in descriptions instead.
use super::{Issue, list};
use serde_json::{Map, Value, json};

/// A closed object whose every property is required.
pub fn object(properties: Vec<(&str, Value)>) -> Value {
    let required: Vec<&str> = properties.iter().map(|(key, _)| *key).collect();
    let properties: Map<String, Value> = properties
        .into_iter()
        .map(|(key, schema)| (key.to_owned(), schema))
        .collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

/// Arguments of a tool that takes none.
pub fn no_arguments() -> Value {
    object(Vec::new())
}

pub fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

pub fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

pub fn number(description: &str) -> Value {
    json!({"type": "number", "description": description})
}

pub fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

/// One of `values`.
pub fn choice(values: &[&str], description: &str) -> Value {
    json!({"type": "string", "enum": values, "description": description})
}

pub fn array(items: Value, description: &str) -> Value {
    json!({"type": "array", "items": items, "description": description})
}

/// `schema` with a description, for nested objects.
pub fn described(mut schema: Value, description: &str) -> Value {
    schema["description"] = json!(description);
    schema
}

/// `schema`, or null for the default.
pub fn nullable(mut schema: Value) -> Value {
    if let Some(kind) = schema["type"].as_str() {
        schema["type"] = json!([kind, "null"]);
    }
    if let Some(values) = schema.get_mut("enum").and_then(Value::as_array_mut)
        && !values.contains(&Value::Null)
    {
        values.push(Value::Null);
    }
    schema
}

/// Check `args` against `schema` for tool `tool`: a missing key, an extra
/// key, a wrong type or a value outside an enum each become an issue on
/// that argument. Whole-number floats such as `2.0` become integers where
/// the schema asks for one, so the returned value deserializes cleanly.
pub fn check(tool: &str, args: &Value, schema: &Value) -> (Value, Vec<Issue>) {
    let mut args = args.clone();
    let mut issues = Vec::new();
    if args.is_object() {
        check_value(tool, "", &mut args, schema, &mut issues);
    } else {
        issues.push(Issue::usage(
            None,
            format!("Pass the {tool} arguments as a JSON object with every key, using null for defaults."),
        ));
    }
    (args, issues)
}

fn check_value(tool: &str, path: &str, value: &mut Value, schema: &Value, issues: &mut Vec<Issue>) {
    let types = types(schema);
    if types.contains(&"integer")
        && !value.is_i64()
        && !value.is_u64()
        && let Some(number) = value.as_f64()
        && number.fract() == 0.0
        && number.abs() < 9_007_199_254_740_992.0
    {
        *value = json!(number as i64);
    }
    let found = json_type(value);
    if !types
        .iter()
        .any(|kind| *kind == found || (*kind == "number" && found == "integer"))
    {
        let expected: Vec<&str> = types.iter().map(|kind| described_type(kind)).collect();
        issues.push(Issue::usage(
            Some(path),
            format!("`{path}` must be {}.", expected.join(" or ")),
        ));
        return;
    }
    if let Some(values) = schema["enum"].as_array()
        && !values.contains(value)
    {
        let names: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
        issues.push(Issue::usage(
            Some(path),
            format!(
                "`{path}` must be one of {}.",
                list(&names).replace(" and ", " or ")
            ),
        ));
        return;
    }
    match value {
        Value::Object(fields) => check_object(tool, path, fields, schema, issues),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                check_value(
                    tool,
                    &format!("{path}[{index}]"),
                    item,
                    &schema["items"],
                    issues,
                );
            }
        }
        _ => {}
    }
}

fn check_object(
    tool: &str,
    path: &str,
    fields: &mut Map<String, Value>,
    schema: &Value,
    issues: &mut Vec<Issue>,
) {
    let empty = Map::new();
    let properties = schema["properties"].as_object().unwrap_or(&empty);
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    for key in fields.keys() {
        if !properties.contains_key(key) {
            let field = join(key);
            issues.push(Issue::usage(
                Some(&field),
                if path.is_empty() {
                    format!("Remove `{field}`; {tool} has no such argument.")
                } else {
                    format!("Remove `{field}`; `{path}` has no such key.")
                },
            ));
        }
    }
    for (key, property) in properties {
        let field = join(key);
        match fields.get_mut(key) {
            Some(value) => check_value(tool, &field, value, property, issues),
            None if types(property).contains(&"null") => issues.push(Issue::usage(
                Some(&field),
                format!("Provide `{field}` (use null for the default)."),
            )),
            None => issues.push(Issue::usage(Some(&field), format!("Provide `{field}`."))),
        }
    }
}

fn types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn described_type(kind: &str) -> &'static str {
    match kind {
        "string" => "text",
        "integer" => "a whole number",
        "number" => "a number",
        "boolean" => "true or false",
        "object" => "an object",
        "array" => "a list",
        _ => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{catalog, catalog_json};

    const KEYWORDS: [&str; 7] = [
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
    ];

    fn name_ok(name: &str) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 64
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    }

    /// Assert the portable subset at `path` and below.
    fn assert_portable(path: &str, schema: &Value) {
        let fields = schema
            .as_object()
            .unwrap_or_else(|| panic!("{path}: not an object"));
        for key in fields.keys() {
            assert!(KEYWORDS.contains(&key.as_str()), "{path}: keyword {key}");
        }
        let kinds = types(schema);
        assert!(!kinds.is_empty(), "{path}: no type");
        for kind in &kinds {
            assert!(
                [
                    "string", "integer", "number", "boolean", "object", "array", "null"
                ]
                .contains(kind),
                "{path}: type {kind}"
            );
        }
        if let Some(text) = schema.get("description") {
            let text = text.as_str().unwrap();
            assert!(text.is_ascii() && text.len() <= 1024, "{path}: description");
        }
        if let Some(values) = schema.get("enum") {
            let values = values
                .as_array()
                .unwrap_or_else(|| panic!("{path}: enum {values}"));
            assert!(
                values.iter().all(|v| v.is_string() || v.is_null()),
                "{path}"
            );
            assert_eq!(
                kinds.contains(&"null"),
                values.contains(&Value::Null),
                "{path}: a nullable enum lists null"
            );
        }
        if kinds.contains(&"object") {
            assert_eq!(schema["additionalProperties"], false, "{path}: open object");
            let properties = schema["properties"].as_object().expect("properties");
            let mut required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|key| key.as_str().unwrap())
                .collect();
            let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
            required.sort_unstable();
            keys.sort_unstable();
            assert_eq!(required, keys, "{path}: every key is required");
            for (key, property) in properties {
                assert!(name_ok(key), "{path}: property name {key}");
                assert!(
                    !matches!(key.as_str(), "secret" | "password"),
                    "{path}: form-only {key} in a schema"
                );
                assert_portable(&format!("{path}.{key}"), property);
            }
        } else {
            assert!(schema.get("properties").is_none(), "{path}");
        }
        if kinds.contains(&"array") {
            assert_portable(&format!("{path}[]"), &schema["items"]);
        } else {
            assert!(schema.get("items").is_none(), "{path}");
        }
    }

    #[test]
    fn every_catalog_schema_is_in_the_portable_subset() {
        let catalog_json = catalog_json();
        let tools = catalog_json.as_array().unwrap();
        assert_eq!(tools.len(), catalog().len());
        let mut names = std::collections::BTreeSet::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(name_ok(name) && names.insert(name), "tool name {name}");
            let description = tool["description"].as_str().unwrap();
            assert!(
                description.is_ascii() && !description.is_empty() && description.len() <= 1024,
                "{name}: description is {} bytes",
                description.len()
            );
            assert!(!tool["title"].as_str().unwrap().is_empty());
            for key in ["kind", "risk", "needs_session", "needs_vision"] {
                assert!(!tool[key].is_null(), "{name}: {key}");
            }
            let schema = &tool["input_schema"];
            assert_eq!(schema["type"], "object", "{name}: the root is one object");
            assert!(schema.get("description").is_none(), "{name}");
            assert_portable(name, schema);
        }
    }

    #[test]
    fn checks_name_the_argument_and_normalize_whole_numbers() {
        let schema = object(vec![
            ("camera", string("camera")),
            ("fresh", nullable(integer("frames"))),
            ("until", choice(&["time", "frames"], "condition")),
            (
                "region",
                nullable(object(vec![("x", number("x")), ("y", number("y"))])),
            ),
            ("names", nullable(array(string("name"), "names"))),
        ]);
        let (args, issues) = check(
            "demo",
            &json!({"camera": "sim:0", "fresh": 2.0, "until": "time", "region": null, "names": null}),
            &schema,
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(args["fresh"], json!(2));
        let (_, issues) = check(
            "demo",
            &json!({"fresh": 1.5, "until": "soon", "region": {"x": 0, "z": 1}, "names": ["a", 3], "extra": 1}),
            &schema,
        );
        let found: Vec<(Option<&str>, &str)> = issues
            .iter()
            .map(|issue| (issue.field.as_deref(), issue.message.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (Some("extra"), "Remove `extra`; demo has no such argument."),
                (Some("camera"), "Provide `camera`."),
                (Some("fresh"), "`fresh` must be a whole number or null."),
                (Some("names[1]"), "`names[1]` must be text."),
                (
                    Some("region.z"),
                    "Remove `region.z`; `region` has no such key."
                ),
                (Some("region.y"), "Provide `region.y`."),
                (Some("until"), "`until` must be one of time or frames."),
            ]
        );
        let (_, issues) = check("demo", &json!([1]), &schema);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].field.is_none());
        let (_, issues) = check("demo", &json!({"camera": "a", "until": "time"}), &schema);
        assert_eq!(
            issues[0].message,
            "Provide `fresh` (use null for the default)."
        );
    }
}
