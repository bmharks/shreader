//! Publishes a message whenever its content is near the visible area.
//!
//! It keeps no "already fired" flag: widget state can be reused by a
//! different story's placeholder at the same position in the tree. The app
//! ignores repeat requests and stops wrapping the placeholder once it's
//! handled.
//!
//! Used to download story images only as the reader scrolls toward them,
//! rather than all at once (a story can have 100+ large images).
//!
//! Inside a `scrollable`, `on_event` receives the visible region as
//! `viewport`, in the same coordinates as this widget's layout, and every
//! scroll, mouse move or redraw passes through. Comparing the two on those
//! events needs no app-level event subscription.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{event, mouse, Element, Event, Length, Rectangle, Size, Vector};

pub struct OnVisible<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    message: Message,
    /// How far outside the visible area still counts as "near"
    margin: f32,
}

/// Wraps `content`; publishes `message` on events while it's within `margin`
/// pixels of the visible area.
pub fn on_visible<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    message: Message,
    margin: f32,
) -> OnVisible<'a, Message, Theme, Renderer> {
    OnVisible {
        content: content.into(),
        message,
        margin,
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for OnVisible<'a, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: iced::advanced::Renderer,
{
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        if layout.bounds().intersects(&viewport.expand(self.margin)) {
            shell.publish(self.message.clone());
        }

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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
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

impl<'a, Message, Theme, Renderer> From<OnVisible<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: OnVisible<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
