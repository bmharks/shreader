//! Empty space exactly the size an image would be shown at.
//!
//! The reader shows a story image at its own size, or scaled down to the
//! column width (keeping its proportions). When an image is released to save
//! memory, this takes its place: content below doesn't move, so the reading
//! position doesn't jump, and the image reappears in the same spot.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Tree, Widget};
use iced::{mouse, Background, Border, Color, Element, Length, Rectangle, Size};

pub struct ImageSpace {
    width: f32,
    height: f32,
}

/// Space for an image of `width` x `height` pixels.
pub fn image_space(width: u32, height: u32) -> ImageSpace {
    ImageSpace {
        width: width.max(1) as f32,
        height: height.max(1) as f32,
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for ImageSpace
where
    Renderer: iced::advanced::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        // Same rule as a shrink-sized image: its own width, or narrower to
        // fit, with the height following the proportions
        let width = self.width.min(limits.max().width);
        layout::Node::new(Size::new(width, width * self.height / self.width))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        // A faint box, so the space doesn't look like a layout glitch
        renderer.fill_quad(
            Quad {
                bounds: layout.bounds(),
                border: Border { radius: 4.0.into(), ..Border::default() },
                ..Quad::default()
            },
            Background::Color(Color::from_rgba(0.5, 0.5, 0.5, 0.08)),
        );
    }
}

impl<'a, Message, Theme, Renderer> From<ImageSpace> for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn from(space: ImageSpace) -> Self {
        Element::new(space)
    }
}
