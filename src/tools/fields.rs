//! Form metadata for each tool's arguments: labels, input kinds, ranges and
//! help, used by the desktop's step cards and the documentation. Keys match
//! the schemas exactly, except form-only (`ui_only`) fields such as secrets,
//! which never appear in a schema.
use super::{FieldKind, FieldSpec};

const fn field(
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    optional: bool,
    help: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        optional,
        help,
        ui_only: false,
    }
}

/// Conditions `wait_for` waits for.
pub const WAIT_CONDITIONS: &[(&str, &str)] = &[
    ("time", "The whole time"),
    ("frames", "New frames"),
    ("auto_settled", "Auto exposure settles"),
    ("job_done", "A scheduled capture finishes"),
    ("recording_stopped", "Recording or streaming stops"),
    ("activity", "Someone else uses the session"),
];

pub const LIST_CAMERAS: &[FieldSpec] = &[
    field(
        "discover",
        "Search for cameras",
        FieldKind::Bool,
        false,
        "Search the network and this computer for about 3 seconds.",
    ),
    field(
        "include_simulated",
        "Include simulated cameras",
        FieldKind::Bool,
        false,
        "List simulator cameras, which make synthetic test images.",
    ),
];

pub const LIST_JOBS: &[FieldSpec] = &[field(
    "camera",
    "Camera",
    FieldKind::Camera,
    true,
    "Only this camera's work; empty lists every camera.",
)];

pub const GET_CAMERA_FEATURES: &[FieldSpec] = &[
    field(
        "camera",
        "Camera",
        FieldKind::Camera,
        false,
        "A connected camera.",
    ),
    field(
        "search",
        "Search",
        FieldKind::Text,
        true,
        "Part of a feature name, such as Exposure.",
    ),
    field(
        "names",
        "Features",
        FieldKind::Features,
        true,
        "Exact feature names to read.",
    ),
];

pub const INSPECT_FRAME: &[FieldSpec] = &[
    field(
        "camera",
        "Camera",
        FieldKind::Camera,
        false,
        "A connected camera.",
    ),
    field(
        "fresh",
        "New frames",
        FieldKind::Integer { min: 0, max: 10 },
        true,
        "Frames to wait for first; 1 by default, 2 after changing settings.",
    ),
    field(
        "max_dimension",
        "Image size",
        FieldKind::Integer { min: 0, max: 1568 },
        true,
        "Longest edge in pixels: 0 for measurements only, else 256 to 1,568; 1,024 by default.",
    ),
    field(
        "region",
        "Region",
        FieldKind::Region,
        true,
        "Part of the frame to crop and measure; the whole frame by default.",
    ),
    field(
        "include_image",
        "Include the image",
        FieldKind::Bool,
        true,
        "Send a downscaled image with the measurements; on by default.",
    ),
];

pub const WAIT_FOR: &[FieldSpec] = &[
    field(
        "camera",
        "Camera",
        FieldKind::Camera,
        true,
        "The camera to watch; needed for frames, auto exposure and recordings.",
    ),
    field(
        "until",
        "Wait until",
        FieldKind::Choice(WAIT_CONDITIONS),
        false,
        "What to wait for.",
    ),
    field(
        "timeout",
        "At most",
        FieldKind::Duration,
        false,
        "How long to wait: 1 second to 10 minutes, such as 30s or 2m.",
    ),
    field(
        "frames",
        "Frames",
        FieldKind::Integer { min: 1, max: 1000 },
        true,
        "New frames to wait for, 1 to 1,000.",
    ),
    field(
        "job_id",
        "Job",
        FieldKind::Integer {
            min: 1,
            max: i64::MAX,
        },
        true,
        "The scheduled capture to wait for, from list_jobs.",
    ),
];

#[cfg(test)]
mod tests {
    use crate::tools::catalog;
    use serde_json::Value;

    #[test]
    fn fields_match_their_schemas() {
        for spec in catalog() {
            let schema = (spec.schema)();
            let properties = schema["properties"].as_object().unwrap();
            let shown: Vec<&str> = spec
                .fields
                .iter()
                .filter(|field| !field.ui_only)
                .map(|field| field.key)
                .collect();
            let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
            let mut sorted = shown.clone();
            sorted.sort_unstable();
            keys.sort_unstable();
            assert_eq!(sorted, keys, "{}: fields and schema keys", spec.name);
            for field in spec.fields.iter().filter(|field| !field.ui_only) {
                let nullable = match &properties[field.key]["type"] {
                    Value::Array(kinds) => kinds.contains(&Value::from("null")),
                    _ => false,
                };
                assert_eq!(
                    field.optional, nullable,
                    "{}.{}: optional fields are nullable",
                    spec.name, field.key
                );
                assert!(!field.label.is_empty() && field.help.ends_with('.'));
            }
        }
    }
}
