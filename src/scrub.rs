//! Redaction of URL credentials in errors and structured results.

use serde_json::Value;

/// `message` with every string in `known` that carries URL credentials
/// replaced by its redacted form, longest first, so an error that quotes a
/// command's own camera or output shows no secret even where it is not a URL.
pub fn redact_error(message: &str, known: &Value) -> String {
    let mut values = Vec::new();
    collect_strings(known, &mut values);
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut message = message.to_owned();
    for value in values {
        let redacted = crate::media::redact_url(&value);
        if redacted != value {
            message = message.replace(&value, &redacted);
        }
    }
    message
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(value) => strings.push(value.clone()),
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_strings(value, strings)),
        Value::Object(fields) => fields
            .values()
            .for_each(|value| collect_strings(value, strings)),
        _ => {}
    }
}

/// Redact URL credentials in every string of `value`, in place.
pub fn redact_value(value: &mut Value) {
    match value {
        Value::String(value) => *value = crate::media::redact_url(value),
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        Value::Object(fields) => fields.values_mut().for_each(redact_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credentials_are_redacted_wherever_a_known_string_appears() {
        let known = json!({"camera":"rtsp://user:secret@cam/live","steps":[{"output":"clip.mp4"}]});
        let message = redact_error(
            "cannot open rtsp://user:secret@cam/live for clip.mp4",
            &known,
        );
        assert!(!message.contains("secret"), "{message}");
        assert!(message.contains("clip.mp4"), "{message}");
        let mut value = known.clone();
        redact_value(&mut value);
        assert!(!value.to_string().contains("secret"), "{value}");
        assert_eq!(value["steps"][0]["output"], "clip.mp4");
    }
}
