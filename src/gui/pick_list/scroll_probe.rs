//! Read iced's actual menu scroll translation through its public renderer
//! contract. This keeps accessibility option bounds aligned with scrollbar and
//! touch scrolling without reaching into the menu's private widget state.
use iced::advanced::{image, renderer, text};
use iced::{Background, Color, Pixels, Point, Rectangle, Transformation};
use std::marker::PhantomData;

pub(super) struct Probe<R: text::Renderer> {
    font: R::Font,
    size: Pixels,
    pub offset: f32,
    renderer: PhantomData<fn() -> R>,
}

impl<R: text::Renderer> Probe<R> {
    pub fn new(renderer: &R) -> Self {
        Self {
            font: renderer.default_font(),
            size: renderer.default_size(),
            offset: 0.0,
            renderer: PhantomData,
        }
    }
}

impl<R: text::Renderer> renderer::Renderer for Probe<R> {
    fn start_layer(&mut self, _: Rectangle) {}
    fn end_layer(&mut self) {}
    fn start_transformation(&mut self, transform: Transformation) {
        self.offset = (-transform.translation().y).max(0.0);
    }
    fn end_transformation(&mut self) {}
    fn fill_quad(&mut self, _: renderer::Quad, _: impl Into<Background>) {}
    fn reset(&mut self, _: Rectangle) {}
    fn allocate_image(
        &mut self,
        _: &image::Handle,
        _: impl FnOnce(Result<image::Allocation, image::Error>) + Send + 'static,
    ) {
        // Native menu rows contain only text; this probe paints no resources.
    }
}

impl<R: text::Renderer> text::Renderer for Probe<R> {
    type Font = R::Font;
    type Paragraph = R::Paragraph;
    type Editor = R::Editor;
    const ICON_FONT: Self::Font = R::ICON_FONT;
    const CHECKMARK_ICON: char = R::CHECKMARK_ICON;
    const ARROW_DOWN_ICON: char = R::ARROW_DOWN_ICON;
    const SCROLL_UP_ICON: char = R::SCROLL_UP_ICON;
    const SCROLL_DOWN_ICON: char = R::SCROLL_DOWN_ICON;
    const SCROLL_LEFT_ICON: char = R::SCROLL_LEFT_ICON;
    const SCROLL_RIGHT_ICON: char = R::SCROLL_RIGHT_ICON;
    const ICED_LOGO: char = R::ICED_LOGO;
    fn default_font(&self) -> Self::Font {
        self.font
    }
    fn default_size(&self) -> Pixels {
        self.size
    }
    fn fill_paragraph(&mut self, _: &Self::Paragraph, _: Point, _: Color, _: Rectangle) {}
    fn fill_editor(&mut self, _: &Self::Editor, _: Point, _: Color, _: Rectangle) {}
    fn fill_text(&mut self, _: text::Text<String, Self::Font>, _: Point, _: Color, _: Rectangle) {}
}
