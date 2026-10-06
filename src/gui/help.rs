//! The quick guide sheet.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(super) fn help(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let section = |(name, actions): (&'static str, &'static [Action])| {
            let mut list = column![heading(name, p)].spacing(7);
            for action in actions
                .iter()
                .filter(|action| !action.bindings(os).is_empty())
            {
                list = list.push(
                    row![
                        text(action.label()).size(style::BODY).width(Fill),
                        text(action.shortcut(os))
                            .size(style::SMALL)
                            .font(style::MEDIUM)
                            .color(p.secondary),
                    ]
                    .spacing(10),
                );
            }
            list
        };
        let [cameras, acquisition, view, window] = Action::SECTIONS;
        let content = column![
            row![
                icon(Icon::Keyboard, 24.0, p.accent),
                text("Quick guide").size(22).font(style::BOLD),
                space::horizontal(),
                button(icon(Icon::Close, 14.0, p.secondary))
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::Help(false)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text("Discover a camera, connect, then start a stream. The simulator is available without camera hardware.")
                .size(style::BODY)
                .color(p.secondary),
            row![
                column![section(cameras), section(acquisition)].spacing(18).width(Fill),
                column![section(view), section(window)].spacing(18).width(Fill),
            ]
            .spacing(32),
            text("Space, Enter and Esc act on the workbench when no field has keyboard focus. Press Esc or click elsewhere to leave a field.")
                .size(style::SMALL)
                .color(p.secondary),
            heading("One visible session, many ways to control it", p),
            text("Copy the session command to attach your terminal or coding agent. Session commands update this window and share its camera connection.")
                .size(style::BODY),
            code_block(self.session_command(), p),
            code_block("capturefab --help".into(), p),
            text("Add stream URLs directly. Native camera sources use avfoundation:// on macOS, v4l2:// on Linux, and dshow:// on Windows. GigE: use a reachable address on the camera's subnet. USB3: the operating system must allow access to the camera. Activity shows connection and capture errors.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(14)
        .padding(28);
        container(scrollable(content).style(style::scroll))
            .width(700)
            .max_height(680)
            .style(style::sheet)
            .into()
    }
}
