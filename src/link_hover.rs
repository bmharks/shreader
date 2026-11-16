//! Shows the pointing-hand cursor over the links in a block of story text.
//!
//! The story text is a read-only `text_editor` (so it can be selected), and
//! the editor doesn't say what's under the mouse. So this wrapper lays out
//! the same text a second time, as plain and link pieces with exactly the
//! editor's font, size, line height and wrapping (hence the same line
//! breaks), and asks that layout which piece the mouse is over.
//!
//! The layout is only made while the mouse is over the block, and remade
//! only when the width changes, so it costs nothing while resizing.

use std::cell::RefCell;

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{alignment, event, mouse, Element, Event, Length, Pixels, Rectangle, Size, Vector};

use crate::story_content::LinkSpan;

pub struct LinkHover<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    text: &'a str,
    links: &'a [LinkSpan],
    size: Pixels,
    wrapping: text::Wrapping,
}

/// Wraps `content`, a text editor showing `text` at `size` with `wrapping`,
/// and shows a pointer over `links`.
pub fn link_hover<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    text: &'a str,
    links: &'a [LinkSpan],
    size: impl Into<Pixels>,
    wrapping: text::Wrapping,
) -> LinkHover<'a, Message, Theme, Renderer> {
    LinkHover {
        content: content.into(),
        text,
        links,
        size: size.into(),
        wrapping,
    }
}

/// The second layout, made on demand, and the width it was made for
struct State<P> {
    layout: RefCell<Option<(f32, P)>>,
}

impl<Message, Theme, Renderer> LinkHover<'_, Message, Theme, Renderer>
where
    Renderer: text::Renderer,
{
    /// The text as pieces, each marked with whether it's a link
    fn spans(&self) -> Vec<text::Span<'_, bool, Renderer::Font>> {
        let mut spans = Vec::new();
        for (number, line) in self.text.split('\n').enumerate() {
            if number > 0 {
                spans.push(piece("\n", false));
            }
            let mut position = 0;
            for link in self.links.iter().filter(|link| link.line == number) {
                if link.start < position || link.end > line.len() {
                    continue; // out of order or out of range: not ours
                }
                spans.push(piece(&line[position..link.start], false));
                spans.push(piece(&line[link.start..link.end], true));
                position = link.end;
            }
            spans.push(piece(&line[position..], false));
        }
        spans.retain(|span| !span.text.is_empty());
        spans
    }
}

fn piece<Font>(text: &str, link: bool) -> text::Span<'_, bool, Font> {
    let mut span = text::Span::new(text);
    span.link = Some(link);
    span
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for LinkHover<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph> { layout: RefCell::new(None) })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget().layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> event::Status {
        self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        )
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        if !self.links.is_empty() {
            if let Some(position) = cursor.position_in(bounds) {
                let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
                let mut cached = state.layout.borrow_mut();
                if cached.as_ref().is_none_or(|(width, _)| *width != bounds.width) {
                    let spans = self.spans();
                    let paragraph = Renderer::Paragraph::with_spans(text::Text {
                        content: spans.as_slice(),
                        bounds: Size::new(bounds.width, f32::INFINITY),
                        size: self.size,
                        line_height: text::LineHeight::default(),
                        font: renderer.default_font(),
                        horizontal_alignment: alignment::Horizontal::Left,
                        vertical_alignment: alignment::Vertical::Top,
                        shaping: text::Shaping::Advanced,
                        wrapping: self.wrapping,
                    });
                    *cached = Some((bounds.width, paragraph));
                }
                if let Some((_, paragraph)) = cached.as_ref() {
                    let spans = self.spans();
                    let on_link = paragraph
                        .hit_span(position)
                        .and_then(|index| spans.get(index))
                        .is_some_and(|span| span.link == Some(true));
                    if on_link {
                        return mouse::Interaction::Pointer;
                    }
                }
            }
        }
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(&mut tree.children[0], layout, renderer, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<LinkHover<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: text::Renderer + 'static,
{
    fn from(widget: LinkHover<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
