//! Line icons drawn as vector paths on a 16-unit grid, so they stay crisp at
//! any size and take their color from the palette.
use iced::widget::canvas::{self, Frame, Geometry, LineCap, LineJoin, Path, Stroke, path::Arc};
use iced::{Color, Element, Point, Radians, Rectangle, Renderer, Size, Theme, mouse};
use std::f32::consts::{FRAC_PI_2, PI};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Stop,
    Camera,
    Refresh,
    Plus,
    Minus,
    Close,
    Grid,
    Chart,
    Activity,
    Help,
    Copy,
    Sun,
    Moon,
    Contrast,
    ChevronRight,
    ChevronDown,
    Broadcast,
    Folder,
    Eject,
    /// The app mark: a rounded lens body.
    Mark,
}

struct Glyph {
    icon: Icon,
    color: Color,
}

pub fn icon<'a, Message: 'a>(icon: Icon, size: f32, color: Color) -> Element<'a, Message> {
    canvas::Canvas::new(Glyph { icon, color })
        .width(size)
        .height(size)
        .into()
}

impl<Message> canvas::Program<Message> for Glyph {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        draw(&mut frame, self.icon, self.color, bounds.size());
        vec![frame.into_geometry()]
    }
}

fn draw(frame: &mut Frame, icon: Icon, color: Color, size: Size) {
    let s = size.width.min(size.height) / 16.0;
    let p = |x: f32, y: f32| Point::new(x * s, y * s);
    let stroke = Stroke::default()
        .with_color(color)
        .with_width(1.4 * s)
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round);
    let arc = |b: &mut canvas::path::Builder, cx: f32, cy: f32, r: f32, from: f32, to: f32| {
        b.arc(Arc {
            center: p(cx, cy),
            radius: r * s,
            start_angle: Radians(from),
            end_angle: Radians(to),
        })
    };
    match icon {
        Icon::Play => frame.fill(
            &Path::new(|b| {
                b.move_to(p(4.5, 2.8));
                b.line_to(p(13.0, 8.0));
                b.line_to(p(4.5, 13.2));
                b.close();
            }),
            color,
        ),
        Icon::Stop => frame.fill(
            &Path::rounded_rectangle(p(3.5, 3.5), Size::new(9.0 * s, 9.0 * s), (2.0 * s).into()),
            color,
        ),
        Icon::Camera => {
            frame.stroke(
                &Path::new(|b| {
                    b.move_to(p(2.0, 5.5));
                    b.line_to(p(5.0, 5.5));
                    b.line_to(p(6.2, 3.5));
                    b.line_to(p(9.8, 3.5));
                    b.line_to(p(11.0, 5.5));
                    b.line_to(p(14.0, 5.5));
                    b.line_to(p(14.0, 13.0));
                    b.line_to(p(2.0, 13.0));
                    b.close();
                }),
                stroke,
            );
            frame.stroke(&Path::circle(p(8.0, 9.0), 2.4 * s), stroke);
        }
        Icon::Refresh => {
            // Nearly a full circle, open at the upper right, with an arrowhead
            // where it ends, pointing along the curve.
            let (start, end, r) = (PI * 0.05, PI * 1.72, 5.2);
            frame.stroke(&Path::new(|b| arc(b, 8.0, 8.0, r, start, end)), stroke);
            let tip = (8.0 + r * end.cos(), 8.0 + r * end.sin());
            let (dx, dy) = (-end.sin(), end.cos());
            let wing = |turn: f32| {
                let (c, s2) = (turn.cos(), turn.sin());
                let (x, y) = (-(dx * c - dy * s2), -(dx * s2 + dy * c));
                p(tip.0 + 3.0 * x, tip.1 + 3.0 * y)
            };
            frame.stroke(
                &Path::new(|b| {
                    b.move_to(wing(0.75));
                    b.line_to(p(tip.0, tip.1));
                    b.line_to(wing(-0.75));
                }),
                stroke,
            );
        }
        Icon::Plus => frame.stroke(
            &Path::new(|b| {
                b.move_to(p(8.0, 3.0));
                b.line_to(p(8.0, 13.0));
                b.move_to(p(3.0, 8.0));
                b.line_to(p(13.0, 8.0));
            }),
            stroke,
        ),
        Icon::Minus => frame.stroke(&Path::line(p(3.0, 8.0), p(13.0, 8.0)), stroke),
        Icon::Close => frame.stroke(
            &Path::new(|b| {
                b.move_to(p(4.0, 4.0));
                b.line_to(p(12.0, 12.0));
                b.move_to(p(12.0, 4.0));
                b.line_to(p(4.0, 12.0));
            }),
            stroke,
        ),
        Icon::Grid => {
            for (x, y) in [(2.5, 2.5), (9.0, 2.5), (2.5, 9.0), (9.0, 9.0)] {
                frame.stroke(
                    &Path::rounded_rectangle(
                        p(x, y),
                        Size::new(4.5 * s, 4.5 * s),
                        (1.2 * s).into(),
                    ),
                    stroke,
                );
            }
        }
        Icon::Chart => frame.stroke(
            &Path::new(|b| {
                for (x, top) in [(3.5, 9.0), (6.5, 4.0), (9.5, 7.0), (12.5, 2.5)] {
                    b.move_to(p(x, 13.5));
                    b.line_to(p(x, top));
                }
            }),
            stroke,
        ),
        Icon::Activity => frame.stroke(
            &Path::new(|b| {
                for (y, end) in [(4.0, 13.0), (8.0, 13.0), (12.0, 9.0)] {
                    b.move_to(p(3.0, y));
                    b.line_to(p(end, y));
                }
            }),
            stroke,
        ),
        Icon::Help => {
            frame.stroke(&Path::circle(p(8.0, 8.0), 6.2 * s), stroke);
            frame.stroke(
                &Path::new(|b| {
                    arc(b, 8.0, 6.4, 1.9, PI, PI * 2.25);
                    b.line_to(p(8.0, 9.2));
                }),
                stroke,
            );
            frame.fill(&Path::circle(p(8.0, 11.6), 0.9 * s), color);
        }
        Icon::Copy => {
            frame.stroke(
                &Path::rounded_rectangle(
                    p(5.5, 5.5),
                    Size::new(8.0 * s, 8.0 * s),
                    (1.6 * s).into(),
                ),
                stroke,
            );
            frame.stroke(
                &Path::new(|b| {
                    b.move_to(p(10.5, 2.5));
                    b.line_to(p(4.0, 2.5));
                    b.arc_to(p(2.5, 2.5), p(2.5, 4.0), 1.5 * s);
                    b.line_to(p(2.5, 10.5));
                }),
                stroke,
            );
        }
        Icon::Sun => {
            frame.stroke(&Path::circle(p(8.0, 8.0), 2.8 * s), stroke);
            frame.stroke(
                &Path::new(|b| {
                    for i in 0..8 {
                        let a = i as f32 * PI / 4.0;
                        b.move_to(p(8.0 + 5.0 * a.cos(), 8.0 + 5.0 * a.sin()));
                        b.line_to(p(8.0 + 6.4 * a.cos(), 8.0 + 6.4 * a.sin()));
                    }
                }),
                stroke,
            );
        }
        Icon::Moon => frame.stroke(
            &Path::new(|b| {
                arc(b, 8.0, 8.0, 5.8, -FRAC_PI_2 * 0.35, PI * 1.62);
                b.bezier_curve_to(p(6.0, 10.5), p(9.5, 5.0), p(13.3, 6.0));
            }),
            stroke,
        ),
        Icon::Contrast => {
            frame.stroke(&Path::circle(p(8.0, 8.0), 5.8 * s), stroke);
            frame.fill(
                &Path::new(|b| {
                    arc(b, 8.0, 8.0, 5.8, -FRAC_PI_2, FRAC_PI_2);
                    b.close();
                }),
                color,
            );
        }
        Icon::ChevronRight => frame.stroke(
            &Path::new(|b| {
                b.move_to(p(6.0, 3.5));
                b.line_to(p(10.5, 8.0));
                b.line_to(p(6.0, 12.5));
            }),
            stroke,
        ),
        Icon::ChevronDown => frame.stroke(
            &Path::new(|b| {
                b.move_to(p(3.5, 6.0));
                b.line_to(p(8.0, 10.5));
                b.line_to(p(12.5, 6.0));
            }),
            stroke,
        ),
        Icon::Broadcast => {
            frame.fill(&Path::circle(p(8.0, 8.0), 1.6 * s), color);
            frame.stroke(
                &Path::new(|b| {
                    for r in [4.0, 6.6] {
                        arc(b, 8.0, 8.0, r, -PI * 0.25, PI * 0.25);
                        b.move_to(p(8.0 - r * (PI * 0.25).cos(), 8.0 - r * (PI * 0.25).sin()));
                        arc(b, 8.0, 8.0, r, PI * 1.25, PI * 0.75);
                    }
                }),
                stroke,
            );
        }
        Icon::Folder => frame.stroke(
            &Path::new(|b| {
                b.move_to(p(2.0, 12.5));
                b.line_to(p(2.0, 3.5));
                b.line_to(p(6.0, 3.5));
                b.line_to(p(7.5, 5.0));
                b.line_to(p(14.0, 5.0));
                b.line_to(p(14.0, 12.5));
                b.close();
            }),
            stroke,
        ),
        Icon::Eject => {
            frame.fill(
                &Path::new(|b| {
                    b.move_to(p(8.0, 3.0));
                    b.line_to(p(13.0, 9.0));
                    b.line_to(p(3.0, 9.0));
                    b.close();
                }),
                color,
            );
            frame.fill(
                &Path::rounded_rectangle(
                    p(3.0, 11.0),
                    Size::new(10.0 * s, 2.0 * s),
                    (0.6 * s).into(),
                ),
                color,
            );
        }
        Icon::Mark => {
            frame.fill(
                &Path::rounded_rectangle(
                    p(0.5, 0.5),
                    Size::new(15.0 * s, 15.0 * s),
                    (4.0 * s).into(),
                ),
                color,
            );
            let white = Stroke {
                width: 1.6 * s,
                ..stroke.with_color(Color::WHITE)
            };
            frame.stroke(&Path::circle(p(8.0, 8.0), 3.8 * s), white);
            frame.fill(&Path::circle(p(8.0, 8.0), 1.3 * s), Color::WHITE);
        }
    }
}
