//! Tab completion in the shell: command and subcommand names, flags,
//! camera IDs, and the default camera's feature names, banks, enum values
//! and canonical controls, from the clap tree and what the shell learned of
//! its cameras.
use super::{BUILTINS, escapable, subcommand};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

/// What completion knows of a feature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Feature {
    pub name: String,
    pub kind: String,
    pub access: String,
    pub choices: Vec<String>,
    pub selectors: Vec<String>,
}
impl Feature {
    pub(super) fn of(row: &Value) -> Option<Self> {
        let strings = |key: &str| {
            row[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        Some(Self {
            name: row["name"].as_str()?.to_owned(),
            kind: row["type"].as_str().unwrap_or_default().to_owned(),
            access: row["access"].as_str().unwrap_or_default().to_owned(),
            choices: strings("choices"),
            selectors: strings("selectors"),
        })
    }
}

/// What completion knows of one camera.
#[derive(Debug, Default)]
pub(super) struct Cache {
    pub features: Vec<Feature>,
    /// More features than one list holds: the rest come by first letter
    /// as completion asks for them.
    pub partial: bool,
    pub letters: BTreeSet<char>,
}
impl Cache {
    /// Keep the first description of each feature, in arrival order.
    pub(super) fn extend(&mut self, rows: Vec<Feature>) {
        if rows.is_empty() {
            return;
        }
        let mut names: HashSet<_> = self.features.iter().map(|row| row.name.as_str()).collect();
        let unique: Vec<_> = rows
            .iter()
            .map(|row| names.insert(row.name.as_str()))
            .collect();
        drop(names);
        self.features.extend(
            rows.into_iter()
                .zip(unique)
                .filter_map(|(row, unique)| unique.then_some(row)),
        );
    }
}

/// `text` as one word of the shell: quoted when it has whitespace, quotes,
/// `#` or a backslash.
fn quote(text: &str) -> String {
    if text.is_empty() || text.chars().any(escapable) {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        text.to_owned()
    }
}

/// The words before the cursor, and the word it is in with where that
/// starts: how completion reads a line, open quotes and all.
fn partial(line: &str) -> (Vec<String>, String, usize) {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut start = line.len();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
            continue;
        }
        if !in_word {
            start = at;
            in_word = true;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '\\' => match chars.peek() {
                Some(&(_, next)) if escapable(next) => {
                    word.push(next);
                    chars.next();
                }
                _ => word.push('\\'),
            },
            c => word.push(c),
        }
    }
    if in_word {
        (words, word, start)
    } else {
        (words, String::new(), line.len())
    }
}

/// What completion offers besides the clap tree.
#[derive(Debug, Default)]
pub(super) struct Hints {
    /// Cameras to name: the shell's, those sessions hold, the simulator.
    pub cameras: Vec<String>,
    /// The default camera's features.
    pub features: Vec<Feature>,
    /// The default camera, whose features these are.
    pub camera: Option<String>,
}

/// What a value of `--set NAME=VALUE`, `set` or `get` may name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Names {
    /// `get`: features and controls.
    Read,
    /// `set`: `NAME=` of features that can be written, and controls, then
    /// their values.
    Assign,
    /// `exec`: command features.
    Run,
}

/// Completions of the line before the cursor: where the word being
/// completed starts, and its candidates.
pub(super) fn complete(tree: &clap::Command, line: &str, hints: &Hints) -> (usize, Vec<String>) {
    let (words, current, start) = partial(line);
    let mut candidates = candidates(tree, &words, &current, hints);
    candidates.sort();
    candidates.dedup();
    (start, candidates)
}

fn candidates(tree: &clap::Command, words: &[String], current: &str, hints: &Hints) -> Vec<String> {
    let starts = |candidate: &str| candidate.starts_with(current);
    let Some(first) = words.first() else {
        return BUILTINS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .chain(commands(tree))
            .filter(|name| starts(name))
            .collect();
    };
    match first.as_str() {
        "use" if words.len() == 1 => {
            return hints
                .cameras
                .iter()
                .map(String::as_str)
                .chain(std::iter::once("none"))
                .filter(|camera| starts(camera))
                .map(quote)
                .collect();
        }
        "help" => {
            let command = subcommand(tree, &words[1..]);
            return command
                .map(commands)
                .unwrap_or_default()
                .into_iter()
                .filter(|name| starts(name))
                .collect();
        }
        _ => {}
    }
    let first = match first.as_str() {
        "show" => "inspect",
        first => first,
    };
    let mut command = tree;
    let mut pending: Option<&clap::Arg> = None;
    let mut positionals_only = false;
    for word in std::iter::once(first).chain(words.iter().skip(1).map(String::as_str)) {
        if pending.take().is_some() {
            continue;
        }
        if word == "--" {
            positionals_only = true;
            continue;
        }
        if !positionals_only
            && let Some(flag) = word.strip_prefix('-')
            && !flag.is_empty()
        {
            let arg = match flag.strip_prefix('-') {
                Some(long) if !long.contains('=') => command
                    .get_arguments()
                    .find(|arg| arg.get_long() == Some(long)),
                Some(_) => None,
                None => {
                    let mut letters = flag.chars();
                    match (letters.next(), letters.next()) {
                        (Some(short), None) => command
                            .get_arguments()
                            .find(|arg| arg.get_short() == Some(short)),
                        _ => None,
                    }
                }
            };
            pending = arg.filter(|arg| takes_value(arg));
            continue;
        }
        if let Some(sub) = command.find_subcommand(word) {
            command = sub;
        }
    }
    if let Some(arg) = pending {
        return values(arg, current, hints);
    }
    if current.starts_with('-') && !positionals_only {
        return command
            .get_arguments()
            .filter(|arg| !arg.is_hide_set())
            .flat_map(|arg| {
                let long = arg.get_long().map(|long| format!("--{long}"));
                let short = arg.get_short().map(|short| format!("-{short}"));
                long.into_iter().chain(short)
            })
            .filter(|flag| starts(flag))
            .collect();
    }
    if command.has_subcommands() {
        return commands(command)
            .into_iter()
            .filter(|name| starts(name))
            .collect();
    }
    match command.get_name() {
        "get" => names(Names::Read, current, hints),
        "set" => names(Names::Assign, current, hints),
        "exec" => names(Names::Run, current, hints),
        "features" => hints
            .features
            .iter()
            .filter(|feature| starts_folded(&feature.name, current))
            .map(|feature| feature.name.clone())
            .collect(),
        "schema" => commands(tree)
            .into_iter()
            .filter(|name| starts(name))
            .collect(),
        _ => command
            .get_arguments()
            .filter(|arg| arg.is_positional())
            .flat_map(choices)
            .filter(|choice| starts(choice))
            .collect(),
    }
}

/// The visible subcommands of `command`, and their visible aliases.
fn commands(command: &clap::Command) -> Vec<String> {
    command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .flat_map(|sub| {
            std::iter::once(sub.get_name().to_owned())
                .chain(sub.get_visible_aliases().map(str::to_owned))
        })
        .collect()
}

fn takes_value(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Set | clap::ArgAction::Append
    )
}

fn choices(arg: &clap::Arg) -> Vec<String> {
    arg.get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect()
}

/// Values for flag `arg`: cameras, assignments, or its choices.
fn values(arg: &clap::Arg, current: &str, hints: &Hints) -> Vec<String> {
    match arg.get_id().as_str() {
        "camera" | "cameras" => hints
            .cameras
            .iter()
            .filter(|camera| camera.starts_with(current))
            .map(|camera| quote(camera))
            .collect(),
        "settings" => names(Names::Assign, current, hints),
        _ => choices(arg)
            .into_iter()
            .filter(|choice| choice.starts_with(current))
            .collect(),
    }
}

/// Whether `name` starts with `prefix`, ignoring case: `expo` finds
/// `ExposureTime`.
fn starts_folded(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len()
        && name
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// Feature and control names, banks, and for assignments values.
fn names(kind: Names, current: &str, hints: &Hints) -> Vec<String> {
    let suffix = if kind == Names::Assign { "=" } else { "" };
    let feature = |name: &str| hints.features.iter().find(|feature| feature.name == name);
    if kind == Names::Assign
        && let Some((name, value)) = current.split_once('=')
    {
        let bare = name.split('[').next().unwrap_or(name);
        let values = match bare {
            "trigger" => {
                let mut values = vec!["off".to_owned(), "software".to_owned()];
                if let Some(source) = feature("TriggerSource") {
                    for line in source
                        .choices
                        .iter()
                        .map(|choice| choice.to_ascii_lowercase())
                        .filter(|choice| choice.starts_with("line"))
                    {
                        values.push(format!("{line}:rising"));
                        values.push(format!("{line}:falling"));
                        values.push(line);
                    }
                }
                values
            }
            "auto_exposure" | "auto_wb" => ["off", "once", "continuous"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "roi" => vec!["full".to_owned()],
            "format" => feature("PixelFormat")
                .map(|format| format.choices.clone())
                .unwrap_or_default(),
            _ => match feature(bare) {
                Some(feature) if feature.kind == "bool" => vec!["true".into(), "false".into()],
                Some(feature) => feature.choices.clone(),
                None => Vec::new(),
            },
        };
        return values
            .into_iter()
            .filter(|choice| starts_folded(choice, value))
            .map(|choice| quote(&format!("{name}={choice}")))
            .collect();
    }
    // A bank: Feature[Selector value], from the values of its selectors.
    if let Some((name, open)) = current.split_once('[') {
        let Some(feature) = feature(name) else {
            return Vec::new();
        };
        return feature
            .selectors
            .iter()
            .filter_map(|selector| hints.features.iter().find(|row| row.name == *selector))
            .flat_map(|selector| selector.choices.iter())
            .filter(|choice| starts_folded(choice, open))
            .map(|choice| format!("{name}[{choice}]{suffix}"))
            .collect();
    }
    let controls = crate::controls::CONTROLS
        .iter()
        .filter(|_| kind != Names::Run)
        .map(|(control, _)| format!("{control}{suffix}"));
    let features = hints
        .features
        .iter()
        .filter(|feature| match kind {
            Names::Read => feature.kind != "command",
            Names::Assign => feature.kind != "command" && feature.access != "ro",
            Names::Run => feature.kind == "command",
        })
        .map(|feature| format!("{}{suffix}", feature.name));
    controls
        .chain(features)
        .filter(|name| starts_folded(name, current))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;

    #[test]
    fn completion_cache_keeps_first_descriptions_in_arrival_order() {
        let feature = |name: &str, kind: &str| Feature {
            name: name.into(),
            kind: kind.into(),
            ..Feature::default()
        };
        let mut cache = Cache::default();
        cache.extend(vec![feature("Gain", "float"), feature("Width", "int")]);
        cache.extend(vec![
            feature("Width", "other"),
            feature("ExposureTime", "float"),
            feature("Gain", "other"),
            feature("ExposureTime", "other"),
            feature("Height", "int"),
        ]);
        cache.extend(Vec::new());
        assert_eq!(
            cache.features,
            [
                feature("Gain", "float"),
                feature("Width", "int"),
                feature("ExposureTime", "float"),
                feature("Height", "int"),
            ]
        );
    }

    #[test]
    fn completion_reads_the_word_under_the_cursor() {
        assert_eq!(partial("set Expo"), (vec!["set".into()], "Expo".into(), 4));
        assert_eq!(
            partial("set exposure=5ms "),
            (vec!["set".into(), "exposure=5ms".into()], String::new(), 17)
        );
        assert_eq!(
            partial("use 'avfoundation:HD Ca"),
            (vec!["use".into()], "avfoundation:HD Ca".into(), 4)
        );
    }

    fn hints() -> Hints {
        let feature = |name: &str, kind: &str, choices: &[&str], selectors: &[&str]| Feature {
            name: name.into(),
            kind: kind.into(),
            access: "rw".into(),
            choices: choices.iter().map(|c| (*c).to_owned()).collect(),
            selectors: selectors.iter().map(|c| (*c).to_owned()).collect(),
        };
        Hints {
            cameras: vec![
                "sim:0".into(),
                "sim:1".into(),
                "avfoundation:HD Camera".into(),
            ],
            features: vec![
                feature("ExposureTime", "float", &[], &[]),
                feature("ExposureAuto", "enum", &["Off", "Once"], &[]),
                feature("PixelFormat", "enum", &["Mono8", "Mono16"], &[]),
                feature("TriggerMode", "enum", &["Off", "On"], &["TriggerSelector"]),
                feature(
                    "TriggerSelector",
                    "enum",
                    &["FrameStart", "AcquisitionStart"],
                    &[],
                ),
                feature("TriggerSource", "enum", &["Software", "Line0"], &[]),
                feature("TriggerSoftware", "command", &[], &["TriggerSelector"]),
                Feature {
                    access: "ro".into(),
                    ..feature("DeviceModelName", "string", &[], &[])
                },
                feature("ReverseX", "bool", &[], &[]),
            ],
            camera: Some("sim:0".into()),
        }
    }

    fn completed(line: &str) -> Vec<String> {
        complete(&Cli::tree(), line, &hints()).1
    }

    #[test]
    fn completion_offers_commands_flags_cameras_and_choices() {
        let first = completed("");
        for name in [
            "use", "cameras", "show", "help", "exit", "capture", "ls", "watch",
        ] {
            assert!(first.contains(&name.to_owned()), "{name}: {first:?}");
        }
        assert_eq!(completed("ca"), ["cameras", "capture"]);
        assert_eq!(completed("preset a"), ["apply"]);
        assert!(completed("capture --").contains(&"--every".to_owned()));
        assert!(
            completed("capture --").contains(&"--json".to_owned()),
            "globals too"
        );
        assert!(completed("capture -").contains(&"-c".to_owned()));
        assert_eq!(completed("capture -c sim"), ["sim:0", "sim:1"]);
        assert_eq!(completed("info --camera av"), ["'avfoundation:HD Camera'"]);
        assert_eq!(completed("use s"), ["sim:0", "sim:1"]);
        assert_eq!(completed("use n"), ["none"]);
        assert_eq!(completed("record --codec h"), ["h264", "hevc"]);
        assert_eq!(completed("completions z"), ["zsh"]);
        assert_eq!(completed("list --kind s"), ["screen", "sim"]);
        assert_eq!(completed("help pre"), ["preset"]);
        assert_eq!(completed("help preset s"), ["save", "show"]);
        // A value already given moves on to the next word.
        assert_eq!(completed("capture -c sim:0 --ev"), ["--every"]);
        assert_eq!(completed("capture -é --ev"), ["--every"]);
        assert_eq!(completed("capture -- -c sim"), [] as [String; 0]);
    }

    #[test]
    fn completion_knows_the_cameras_features_controls_and_values() {
        assert_eq!(
            completed("get Expo"),
            ["ExposureAuto", "ExposureTime", "exposure"]
        );
        assert_eq!(
            completed("get trig"),
            ["TriggerMode", "TriggerSelector", "TriggerSource", "trigger"]
        );
        assert_eq!(completed("set ExposureA"), ["ExposureAuto="]);
        // Read-only features are not offered to set.
        assert_eq!(completed("set Device"), [] as [String; 0]);
        assert_eq!(completed("get Device"), ["DeviceModelName"]);
        assert_eq!(
            completed("set ExposureAuto=O"),
            ["ExposureAuto=Off", "ExposureAuto=Once"]
        );
        assert_eq!(
            completed("set ReverseX="),
            ["ReverseX=false", "ReverseX=true"]
        );
        assert_eq!(completed("set format=Mono1"), ["format=Mono16"]);
        assert_eq!(
            completed("set auto_exposure="),
            [
                "auto_exposure=continuous",
                "auto_exposure=off",
                "auto_exposure=once"
            ]
        );
        assert_eq!(
            completed("set trigger=line0"),
            [
                "trigger=line0",
                "trigger=line0:falling",
                "trigger=line0:rising"
            ]
        );
        assert_eq!(completed("get TriggerMode[F"), ["TriggerMode[FrameStart]"]);
        assert_eq!(
            completed("set TriggerMode[Frame"),
            ["TriggerMode[FrameStart]="]
        );
        assert_eq!(
            completed("set TriggerMode[FrameStart]=O"),
            ["TriggerMode[FrameStart]=Off", "TriggerMode[FrameStart]=On"]
        );
        assert_eq!(completed("exec Trig"), ["TriggerSoftware"]);
        assert_eq!(
            completed("inspect --set expo"),
            ["ExposureAuto=", "ExposureTime=", "exposure="]
        );
        assert_eq!(completed("show --set Pixel"), ["PixelFormat="]);
        assert_eq!(
            completed("show -c sim:0 --set Pixel"),
            completed("inspect -c sim:0 --set Pixel")
        );
        assert_eq!(completed("features Pix"), ["PixelFormat"]);
        assert_eq!(completed("schema ins"), ["inspect"]);
    }
}
