//! A pass-through overlay that draws its content next to the mouse cursor.
//!
//! The cursor position is read at draw time, so moving the mouse only triggers
//! a redraw. It never produces an app `Message`, so `update`/`view`/layout
//! don't run on every mouse movement during a drag. It can also report a left
//! button release anywhere in the window via `on_release`, which avoids a
//! global event subscription (those receive every mouse move too).

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{event, mouse, window, Element, Event, Length, Rectangle, Size, Vector};

pub struct FollowCursor<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    offset: Vector,
    on_release: Option<Message>,
}

/// Wraps `content` so it's drawn at the cursor position plus `offset`.
/// Fills its parent; meant to be the top layer of a `stack`.
pub fn follow_cursor<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    offset: Vector,
) -> FollowCursor<'a, Message, Theme, Renderer> {
    FollowCursor {
        content: content.into(),
        offset,
        on_release: None,
    }
}

impl<'a, Message, Theme, Renderer> FollowCursor<'a, Message, Theme, Renderer> {
    /// Message to publish when the left mouse button is released anywhere.
    pub fn on_release(mut self, message: Message) -> Self {
        self.on_release = Some(message);
        self
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for FollowCursor<'a, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: iced::advanced::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self.content.as_widget().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(Size::ZERO, limits.max()),
        );
        layout::Node::with_children(limits.max(), vec![child])
    }

    fn on_event(
        &mut self,
        _tree: &mut Tree,
        event: Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(message) = &self.on_release {
                    shell.publish(message.clone());
                }
            }
            _ => {}
        }
        // Never capture anything: widgets underneath must keep receiving events
        event::Status::Ignored
    }

    // Returning `Interaction::None` (the default) matters: `stack` hides the
    // cursor from lower layers when an upper layer reports any interaction.
    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::None
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let Some(pos) = cursor.position() else {
            return;
        };
        let bounds = layout.bounds();
        let Some(child_layout) = layout.children().next() else {
            return;
        };
        let child_bounds = child_layout.bounds();

        // Keep the badge inside the window
        let x = (pos.x + self.offset.x)
            .min(bounds.x + bounds.width - child_bounds.width)
            .max(bounds.x);
        let y = (pos.y + self.offset.y)
            .min(bounds.y + bounds.height - child_bounds.height)
            .max(bounds.y);

        renderer.with_translation(
            Vector::new(x - child_bounds.x, y - child_bounds.y),
            |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    child_layout,
                    mouse::Cursor::Unavailable,
                    &child_bounds,
                );
            },
        );
    }
}

impl<'a, Message, Theme, Renderer> From<FollowCursor<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: FollowCursor<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
