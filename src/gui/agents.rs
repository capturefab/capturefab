//! Connect an external harness to the visible session without provider setup.
use super::*;
use iced::widget::column;

/// A shell invocation for the running application, including app bundles and
/// installation paths with spaces. Windows instructions target PowerShell.
fn argument(value: &str, windows: bool) -> String {
    if windows {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        shell_quote(value)
    }
}

fn executable(path: &str, windows: bool) -> String {
    if windows {
        format!("& {}", argument(path, windows))
    } else {
        shell_quote(path)
    }
}

fn command_prefix(binary: &str, session: &str, windows: bool) -> String {
    let session = argument(session, windows);
    format!(
        "{} --session={session} --actor capture-agent --actor-kind agent",
        executable(binary, windows)
    )
}

/// Captured once with the workbench, never read from the environment by views.
/// Local shell instructions are not part of assistant requests or preferences.
pub(super) struct CommandContext {
    binary: Option<String>,
    session_directory: Option<String>,
    windows: bool,
}

pub(super) struct PlanPreview {
    pub(super) command: String,
    pub(super) capture_root: String,
    pub(super) shell: &'static str,
}

impl Default for CommandContext {
    fn default() -> Self {
        Self::capture(
            std::env::current_exe().ok(),
            std::env::var_os("CAPTUREFAB_SESSION_DIR").map(PathBuf::from),
            cfg!(target_os = "windows"),
        )
    }
}

impl CommandContext {
    fn capture(binary: Option<PathBuf>, directory: Option<PathBuf>, windows: bool) -> Self {
        let mut binary = binary.and_then(|path| path.into_os_string().into_string().ok());
        let session_directory = directory
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| {
                std::path::absolute(path)
                    .ok()
                    .and_then(|path| path.into_os_string().into_string().ok())
            });
        if session_directory.as_ref().is_some_and(Option::is_none) {
            binary = None;
        }
        Self {
            binary,
            session_directory: session_directory.flatten(),
            windows,
        }
    }
    pub(super) fn prefix(&self, session: &str) -> Option<String> {
        let command = command_prefix(self.binary.as_deref()?, session, self.windows);
        Some(match &self.session_directory {
            Some(directory) if self.windows => format!(
                "Remove-Item Env:CAPTUREFAB_CAMERA -ErrorAction SilentlyContinue; $env:CAPTUREFAB_SESSION_DIR = {}; {command}",
                argument(directory, true)
            ),
            Some(directory) => format!(
                "CAPTUREFAB_SESSION_DIR={} env -u CAPTUREFAB_CAMERA {command}",
                argument(directory, false)
            ),
            None if self.windows => format!(
                "Remove-Item Env:CAPTUREFAB_SESSION_DIR -ErrorAction SilentlyContinue; Remove-Item Env:CAPTUREFAB_CAMERA -ErrorAction SilentlyContinue; {command}"
            ),
            None => format!("env -u CAPTUREFAB_SESSION_DIR -u CAPTUREFAB_CAMERA {command}"),
        })
    }

    pub(super) fn plan_preview(
        &self,
        session: &str,
        path: &std::path::Path,
        root: &std::path::Path,
    ) -> Option<PlanPreview> {
        let absolute = |path: &std::path::Path| {
            let value = path.to_str()?;
            let rooted = if self.windows {
                value.starts_with("\\\\") || value.starts_with("//") || {
                    let bytes = value.as_bytes();
                    bytes.len() >= 3
                        && bytes[0].is_ascii_alphabetic()
                        && bytes[1] == b':'
                        && matches!(bytes[2], b'\\' | b'/')
                }
            } else {
                // A POSIX shell's idea of absolute, whichever host builds it.
                value.starts_with('/')
            };
            rooted.then_some(value.to_owned())
        };
        // Changing cwd must not reinterpret either reviewed outputs or the
        // exported document. Neither locator is resolved by a view.
        let path = absolute(path)?;
        let root = absolute(root)?;
        let invocation = format!(
            "{} tools apply {} --dry-run",
            self.prefix(session)?,
            argument(&path, self.windows)
        );
        let command = if self.windows {
            format!(
                "& {{ Push-Location -LiteralPath {} -ErrorAction Stop; try {{ {invocation} }} finally {{ Pop-Location }} }}",
                argument(&root, true)
            )
        } else {
            format!("(cd {} && {invocation})", argument(&root, false))
        };
        Some(PlanPreview {
            command,
            capture_root: root,
            shell: self.shell_name(),
        })
    }

    pub(super) fn shell_name(&self) -> &'static str {
        if self.windows {
            "PowerShell"
        } else {
            "bash or zsh"
        }
    }

    #[cfg(test)]
    pub(super) fn fixture(binary: Option<&str>, directory: Option<&str>, windows: bool) -> Self {
        Self {
            binary: binary.map(str::to_owned),
            session_directory: directory.map(str::to_owned),
            windows,
        }
    }
}

fn briefing(prefix: &str) -> String {
    format!(
        "Help me use Capturefab in its current desktop session.\n\n\
         Start by reading the session and tool catalog:\n\
         {prefix} tools call get_overview --json\n\
         {prefix} tools list --compact --json\n\n\
         Use `tools describe NAME --json` to read each tool's exact arguments, \
         then `tools call NAME --args JSON --json`. Every argument key is required; \
         use null for defaults. Camera tools select the camera in their arguments, \
         so do not add global --camera.\n\n\
         Keep using this session and actor attribution so I can follow your work \
         in Activity. Replace capture-agent with a distinct name for your harness \
         if other agents share this session. Clear CAPTUREFAB_CAMERA from your \
         harness environment before calling camera tools. Ask me for the capture goal, camera or data source, dataset and output destination \
         if I have not given them. Inspect before changing settings. Dry-run \
         actions, review their checks and obtain my approval for Disruptive or HumanOnly actions \
         before using --confirmed-by-user. Never put credentials in arguments or prompts.\n\n\
         Read inspection image paths with your image viewer. Verify saved captures \
         and report their paths, failures and any jobs still running. For sensor tasks, \
         discover open_dataset, start_signal_capture, wait_for_data, query_dataset and export_dataset. \
         Use finite duration, sample and payload limits; verify written counts, final flush, \
         drops and export checksums. Use the acknowledged source capture and dataset slot \
         identities to preserve other participants’ work. If a request \
         times out, inspect status before retrying a write. Pause when I need to \
         intervene, then re-read the session before continuing."
    )
}

impl Workbench {
    pub(super) fn show_agent_setup(&mut self, open: bool) {
        if open && !self.help_open && self.sheet_open() {
            return;
        }
        self.help_open = open;
        self.sheets.agents = open;
        self.sheets.scrolled = false;
    }

    pub(super) fn agent_setup(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let commands = &self.assistant.commands;
        let Some(prefix) = commands.prefix(&self.session) else {
            return sheet_frame("Connect an agent", Message::AgentSetup(false),
                text("Terminal commands are unavailable because this app's executable or session directory could not be resolved safely.")
                    .size(style::BODY).wrapping(text::Wrapping::WordOrGlyph), p)
                .mark(Icon::Keyboard).size(700.0, height).into();
        };
        let mut brief = briefing(&prefix);
        if commands.session_directory.is_some() {
            brief.push_str("\n\nThese commands set CAPTUREFAB_SESSION_DIR to this app's custom session directory before connecting.");
        }
        let copied = self.just_copied(&brief);
        let description = |value: &'static str| {
            text(value)
                .size(style::BODY)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph)
        };
        let mut body = column![
            heading("Give your agent this workbench", p),
            description("Use Claude Code, Codex, Antigravity or another harness that can run local commands. Your harness supplies its own model connection; no Capturefab API key is needed."),
            container(
                column![
                    text(format!("Session: {}", self.session))
                        .size(style::BODY)
                        .font(style::SEMIBOLD),
                    description("Paste the briefing into your harness, then describe what you want to capture. It includes this app's executable path and session."),
                    button(
                        row![
                            icon(if copied { Icon::Check } else { Icon::Copy }, 15.0, Color::WHITE),
                            text(if copied { "Briefing copied" } else { "Copy agent briefing" }).size(style::BODY),
                        ].spacing(8).align_y(Alignment::Center)
                    )
                    .name("Copy agent briefing")
                    .description("Copy instructions and read-only commands for this running session")
                    .style(style::primary)
                    .padding([8, 12])
                    .on_press(Message::Copy(brief)),
                ].spacing(10)
            ).padding(14).width(Fill).style(style::well),
            heading("Check the connection", p),
            description("Run this in a terminal on the same computer and under the same user account. Keep this window open. Commands use the running application even when capturefab is not on PATH."),
            self.copyable(format!("{prefix} status --json"), p),
            text(format!("Commands are for {}.", commands.shell_name()))
                .size(style::SMALL).color(p.secondary),
        ].spacing(14);
        if commands.session_directory.is_some() {
            body = body.push(
                text("Commands include this app's custom CAPTUREFAB_SESSION_DIR.")
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::WordOrGlyph),
            );
        }
        let body = body.extend([
            heading("Step in when needed", p),
            description("Open Activity to follow the agent's actions and saved files. Pause your harness before editing camera controls, then ask it to inspect the session again. Captures, schedules and recordings already accepted can keep running after a harness stops; check their status.").into(),
            heading("Prefer to ask inside Capturefab?", p),
            description("Quick Command uses your chosen provider and API key or local model. Review a plan first, approve each change, or run an image or data task with limits.").into(),
            button(text("Open Quick Command…").size(style::BODY))
                .name("Open built-in assistant from agent setup")
                .style(style::secondary)
                .padding([7, 12])
                .on_press(Message::Assistant(assistant::Input::Quick(assistant::quick::Input::Open(true)))).into(),
        ]);
        sheet_frame("Connect an agent", Message::AgentSetup(false), body, p)
            .mark(Icon::Keyboard)
            .size(700.0, height)
            .scrolled(self.sheets.scrolled)
            .on_scroll(Message::HelpScrolled)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_keeps_copy_and_close_visible_and_reveals_lower_actions_by_keyboard() {
        use crate::gui::destinations::layout_tests::{
            assert_focus_traversal_reveals, prepare_artifact_frame, render_artifact_themed,
            renderer,
        };
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let mut renderer = renderer();
        let app = Workbench::new(SessionHandle::new(), "inspection-bench".into(), true, None);
        for dark in [false, true] {
            for scale in UiScale::ALL {
                for physical in [Size::new(720.0, 820.0), Size::new(480.0, 620.0)] {
                    let size = physical / scale.factor();
                    let mut view = app.agent_setup(Palette::of(dark), size.height);
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, size),
                    );
                    prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                    let (snapshot, bridge, window) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    for label in ["Copy agent briefing", "Close Connect an agent"] {
                        let (id, control) = snapshot
                            .nodes
                            .iter()
                            .find(|(_, control)| control.label() == Some(label))
                            .unwrap_or_else(|| panic!("{label} is exposed"));
                        let bounds = control.bounds().unwrap();
                        assert!(
                            bounds.x0 >= 0.0
                                && bounds.y0 >= 0.0
                                && bounds.x1 <= f64::from(size.width) + 0.5
                                && bounds.y1 <= f64::from(size.height) + 0.5,
                            "{label} at {scale}/{physical:?}: {bounds:?}"
                        );
                        if label == "Copy agent briefing" {
                            let task = bridge.dispatch(accessibility::Request {
                                window,
                                replay: None,
                                action: accesskit::ActionRequest {
                                    action: accesskit::Action::Click,
                                    target: *id,
                                    data: None,
                                },
                            });
                            let mut stream = iced_runtime::task::into_stream(task).unwrap();
                            let output = iced::futures::executor::block_on(stream.next()).unwrap();
                            let iced_runtime::Action::Output(Message::Copy(brief)) = output else {
                                panic!("briefing activation only copies text");
                            };
                            assert!(brief.contains("--session=inspection-bench"));
                            assert!(brief.contains("tools list --compact --json"));
                            assert!(brief.contains("Disruptive or HumanOnly"));
                        }
                    }
                    render_artifact_themed(
                        &format!(
                            "agent-setup-{}-{scale}-{}",
                            if dark { "dark" } else { "light" },
                            physical.width
                        ),
                        &view,
                        &tree,
                        &node,
                        &mut renderer,
                        size,
                        (dark, scale.factor()),
                    );
                    assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
                }
            }
        }
    }

    #[test]
    fn agent_commands_quote_the_actual_binary_and_session_for_each_shell() {
        assert_eq!(
            command_prefix(
                "/Applications/Capturefab App/capturefab",
                "bench's camera",
                false
            ),
            "'/Applications/Capturefab App/capturefab' --session='bench'\\''s camera' --actor capture-agent --actor-kind agent"
        );
        assert_eq!(
            command_prefix(
                "C:\\Program Files\\Capturefab's\\capturefab.exe",
                "bench's camera",
                true
            ),
            "& 'C:\\Program Files\\Capturefab''s\\capturefab.exe' --session='bench''s camera' --actor capture-agent --actor-kind agent"
        );
    }

    #[test]
    fn leading_dash_session_name_is_attached_and_parses_as_the_session_value() {
        use clap::Parser;
        crate::ipc::validate_name("--bench").unwrap();
        let prefix = command_prefix("/opt/capturefab", "--bench", false);
        assert!(prefix.contains("--session=--bench"));
        let cli =
            crate::cli::Cli::try_parse_from(prefix.split_ascii_whitespace().chain(["status"]))
                .expect("an allowed leading-dash session remains a value, not an option");
        assert_eq!(cli.session.as_deref(), Some("--bench"));
        assert!(
            command_prefix("C:\\capturefab.exe", "--bench", true).contains("--session='--bench'")
        );
    }

    #[test]
    fn exported_plan_preview_quotes_every_local_locator_and_checks_before_running() {
        let posix = CommandContext::fixture(
            Some("/Applications/Capturefab's App/capturefab"),
            Some("/tmp/session's $(directory);"),
            false,
        );
        assert_eq!(
            posix
                .plan_preview(
                    "bench's $(name);",
                    std::path::Path::new("/tmp/plan's $(file);.json"),
                    std::path::Path::new("/Volumes/Run's $(root);")
                )
                .unwrap()
                .command,
            "(cd '/Volumes/Run'\\''s $(root);' && CAPTUREFAB_SESSION_DIR='/tmp/session'\\''s $(directory);' env -u CAPTUREFAB_CAMERA '/Applications/Capturefab'\\''s App/capturefab' --session='bench'\\''s $(name);' --actor capture-agent --actor-kind agent tools apply '/tmp/plan'\\''s $(file);.json' --dry-run)"
        );
        let windows = CommandContext::fixture(
            Some("C:\\Program Files\\Capturefab's\\capturefab.exe"),
            Some("C:\\Session's $files;"),
            true,
        );
        assert_eq!(
            windows
                .plan_preview(
                    "bench's $(name);",
                    std::path::Path::new("C:\\plan's $(file);.json"),
                    std::path::Path::new("D:\\Run's $root;")
                )
                .unwrap()
                .command,
            "& { Push-Location -LiteralPath 'D:\\Run''s $root;' -ErrorAction Stop; try { Remove-Item Env:CAPTUREFAB_CAMERA -ErrorAction SilentlyContinue; $env:CAPTUREFAB_SESSION_DIR = 'C:\\Session''s $files;'; & 'C:\\Program Files\\Capturefab''s\\capturefab.exe' --session='bench''s $(name);' --actor capture-agent --actor-kind agent tools apply 'C:\\plan''s $(file);.json' --dry-run } finally { Pop-Location } }"
        );
        assert_eq!(windows.shell_name(), "PowerShell");
        assert_eq!(posix.shell_name(), "bash or zsh");
        assert!(
            CommandContext::fixture(None, None, false)
                .plan_preview(
                    "bench",
                    std::path::Path::new("/tmp/plan.json"),
                    std::path::Path::new("/tmp/output")
                )
                .is_none()
        );
    }

    #[test]
    fn relative_session_directory_is_captured_at_setup_for_commands_from_another_folder() {
        let directory = PathBuf::from("relative sessions").join("bench's $(name);");
        let expected = std::env::current_dir().unwrap().join(&directory);
        let commands = CommandContext::capture(
            Some(PathBuf::from("/Applications/Capturefab App/capturefab")),
            Some(directory),
            false,
        );
        assert_eq!(commands.session_directory.as_deref(), expected.to_str());
        assert!(expected.is_absolute());
        let preview = commands
            .plan_preview(
                "current-bench",
                std::path::Path::new("/tmp/plan.json"),
                std::path::Path::new("/tmp/output"),
            )
            .unwrap();
        assert!(preview.command.contains(&format!(
            "CAPTUREFAB_SESSION_DIR={} ",
            shell_quote(expected.to_str().unwrap())
        )));
        assert!(
            preview
                .command
                .ends_with("tools apply /tmp/plan.json --dry-run)")
        );
    }

    #[test]
    fn default_session_commands_clear_a_terminals_unrelated_directory_override() {
        let path = std::path::Path::new("/tmp/plan.json");
        assert_eq!(
            CommandContext::fixture(Some("/opt/Capturefab App/capturefab"), None, false)
                .plan_preview("bench", path, std::path::Path::new("/tmp/output"))
                .unwrap()
                .command,
            "(cd /tmp/output && env -u CAPTUREFAB_SESSION_DIR -u CAPTUREFAB_CAMERA '/opt/Capturefab App/capturefab' --session=bench --actor capture-agent --actor-kind agent tools apply /tmp/plan.json --dry-run)"
        );
        let empty_override = CommandContext::capture(
            Some(PathBuf::from("/opt/capturefab")),
            Some(PathBuf::new()),
            false,
        );
        assert!(
            empty_override.session_directory.is_none(),
            "an empty override uses the IPC default, not the launch directory"
        );
        assert!(
            empty_override
                .prefix("bench")
                .unwrap()
                .starts_with("env -u CAPTUREFAB_SESSION_DIR -u CAPTUREFAB_CAMERA ")
        );
        assert_eq!(
            CommandContext::fixture(Some("C:\\Program Files\\capturefab.exe"), None, true)
                .plan_preview(
                    "bench",
                    std::path::Path::new("C:\\plan.json"),
                    std::path::Path::new("D:\\output")
                )
                .unwrap()
                .command,
            "& { Push-Location -LiteralPath 'D:\\output' -ErrorAction Stop; try { Remove-Item Env:CAPTUREFAB_SESSION_DIR -ErrorAction SilentlyContinue; Remove-Item Env:CAPTUREFAB_CAMERA -ErrorAction SilentlyContinue; & 'C:\\Program Files\\capturefab.exe' --session='bench' --actor capture-agent --actor-kind agent tools apply 'C:\\plan.json' --dry-run } finally { Pop-Location } }"
        );
    }

    #[cfg(unix)]
    #[test]
    fn plan_preview_runs_from_the_reviewed_folder_and_restores_the_callers_directory() {
        use std::os::unix::fs::PermissionsExt;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temporary = std::env::temp_dir().join(format!("capturefab-preview-{unique}"));
        let caller = temporary.join("terminal cwd");
        let root = temporary.join("reviewed root's $(literal);");
        std::fs::create_dir_all(&caller).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let binary = temporary.join("Capturefab's $(binary);");
        std::fs::write(&binary, b"#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"${CAPTUREFAB_SESSION_DIR-unset}\" \"${CAPTUREFAB_CAMERA-unset}\" \"$@\"\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan = temporary.join("exported plan's $(file);.json");
        for custom_directory in [None, Some("/tmp/session's $(literal);")] {
            let commands = CommandContext::fixture(binary.to_str(), custom_directory, false);
            let preview = commands
                .plan_preview("bench's $(name);", &plan, &root)
                .unwrap();
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    &format!("{}; printf 'CALLER=%s\\n' \"$PWD\"", preview.command),
                ])
                .current_dir(&caller)
                .env("CAPTUREFAB_SESSION_DIR", "/wrong/session")
                .env("CAPTUREFAB_CAMERA", "wrong-camera")
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output.stderr);
            let actual = String::from_utf8(output.stdout).unwrap();
            let rows = actual.lines().collect::<Vec<_>>();
            // macOS can report /private/tmp instead of the equivalent /tmp.
            assert_eq!(
                std::fs::canonicalize(rows[0]).unwrap(),
                std::fs::canonicalize(&root).unwrap()
            );
            assert_eq!(rows[1], custom_directory.unwrap_or("unset"));
            assert_eq!(rows[2], "unset");
            assert_eq!(
                &rows[3..rows.len() - 1],
                &[
                    "--session=bench's $(name);",
                    "--actor",
                    "capture-agent",
                    "--actor-kind",
                    "agent",
                    "tools",
                    "apply",
                    plan.to_str().unwrap(),
                    "--dry-run",
                ]
            );
            assert_eq!(
                std::fs::canonicalize(rows.last().unwrap().strip_prefix("CALLER=").unwrap())
                    .unwrap(),
                std::fs::canonicalize(&caller).unwrap()
            );
            assert!(
                commands
                    .plan_preview("bench", std::path::Path::new("relative-plan.json"), &root)
                    .is_none()
            );
            assert!(
                commands
                    .plan_preview("bench", &plan, std::path::Path::new("relative-root"))
                    .is_none()
            );
            let missing = commands
                .plan_preview("bench", &plan, &temporary.join("missing"))
                .unwrap();
            let failed = std::process::Command::new("/bin/sh")
                .args(["-c", &missing.command])
                .current_dir(&caller)
                .output()
                .unwrap();
            assert!(!failed.status.success());
            assert!(
                failed.stdout.is_empty(),
                "an unavailable output folder must not run from the terminal folder"
            );
        }
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn setup_preserves_other_sheets_and_hands_off_to_the_assistant() {
        let mut app = Workbench::new(SessionHandle::new(), "bench".into(), true, None);
        app.capture_to.manager_open = true;
        let _ = app.handle_message(Message::AgentSetup(true));
        assert!(!app.help_open);
        app.capture_to.manager_open = false;
        let _ = app.handle_message(Message::AgentSetup(true));
        assert!(app.help_open && app.sheets.agents);
        let _ = app.update_assistant(assistant::Input::Quick(assistant::quick::Input::Open(true)));
        assert!(!app.help_open && app.assistant.quick.open);
        let _ = app.update_assistant(assistant::Input::Quick(assistant::quick::Input::Open(
            false,
        )));
        let _ = app.handle_message(Message::Help(true));
        assert!(app.help_open && !app.sheets.agents);
    }
}
