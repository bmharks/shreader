//! Keyboard shortcuts for a whole view, without a global event subscription.
//!
//! `on_key_press` wraps content and offers each key press to its children
//! first. Only keys nobody used (a focused text field captures the arrow keys
//! to move its cursor, for example) are passed to the mapping function.
//!
//! The app avoids `iced::event::listen_with` / `keyboard::on_key_press`: those
//! subscriptions are sent every window event and can overflow their channel
//! ("TrySendError { kind: Full }").
//!
//! Also here: `scroll_into_view`, which scrolls a `scrollable` just enough
//! for a container with a given id to be fully visible.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, operation, Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{container, scrollable};
use iced::{event, mouse, Element, Event, Length, Rectangle, Size, Task, Vector};

pub struct OnKeyPress<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_key: Box<dyn Fn(&Key, Modifiers) -> Option<Message> + 'a>,
}

/// Wraps `content`; key presses its children ignore go to `on_key`, which
/// returns the message to publish, if any.
pub fn on_key_press<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    on_key: impl Fn(&Key, Modifiers) -> Option<Message> + 'a,
) -> OnKeyPress<'a, Message, Theme, Renderer> {
    OnKeyPress {
        content: content.into(),
        on_key: Box::new(on_key),
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for OnKeyPress<'a, Message, Theme, Renderer>
where
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
        let pressed = match &event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some((key.clone(), *modifiers))
            }
            _ => None,
        };

        let status = self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if let (event::Status::Ignored, Some((key, modifiers))) = (status, pressed) {
            if let Some(message) = (self.on_key)(&key, modifiers) {
                shell.publish(message);
                return event::Status::Captured;
            }
        }
        status
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

impl<'a, Message, Theme, Renderer> From<OnKeyPress<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: OnKeyPress<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

/// Space left above or below the item when scrolling to it.
const SCROLL_MARGIN: f32 = 8.0;

/// Scrolls the scrollable `scroll_id` by the least amount that makes the
/// container `item_id` (somewhere inside it) fully visible. Does nothing if
/// it already is, or if either isn't in the view.
pub fn scroll_into_view<Message: Send + 'static>(
    scroll_id: scrollable::Id,
    item_id: container::Id,
) -> Task<Message> {
    let find = FindItem {
        scroll_id: scroll_id.clone().into(),
        item_id: item_id.into(),
        scroll: None,
        item: None,
    };
    iced::advanced::widget::operate(find).then(move |offset: Option<f32>| match offset {
        Some(y) => scrollable::scroll_to(scroll_id.clone(), scrollable::AbsoluteOffset { x: 0.0, y }),
        None => Task::none(),
    })
}

/// Finds the scrollable and the item; its outcome is the vertical offset to
/// scroll to, or `None` if no scrolling is needed.
struct FindItem {
    scroll_id: widget::Id,
    item_id: widget::Id,
    /// (viewport bounds, content bounds, current translation)
    scroll: Option<(Rectangle, Rectangle, Vector)>,
    item: Option<Rectangle>,
}

impl Operation<Option<f32>> for FindItem {
    fn container(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        operate_on_children: &mut dyn FnMut(&mut dyn Operation<Option<f32>>),
    ) {
        if self.item.is_some() {
            return;
        }
        if id == Some(&self.item_id) {
            self.item = Some(bounds);
            return;
        }
        operate_on_children(self);
    }

    fn scrollable(
        &mut self,
        _state: &mut dyn operation::Scrollable,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
    ) {
        if id == Some(&self.scroll_id) {
            self.scroll = Some((bounds, content_bounds, translation));
        }
    }

    fn finish(&self) -> operation::Outcome<Option<f32>> {
        let (Some((viewport, content, translation)), Some(item)) = (self.scroll, self.item) else {
            return operation::Outcome::Some(None);
        };
        // The item's position within the scrolled content (layouts inside a
        // scrollable aren't translated)
        let top = item.y - content.y;
        let bottom = top + item.height;
        let visible_top = translation.y;
        let visible_bottom = translation.y + viewport.height;

        let offset = if top < visible_top {
            Some((top - SCROLL_MARGIN).max(0.0))
        } else if bottom > visible_bottom {
            // A card taller than the viewport lines up with its top
            Some((bottom - viewport.height + SCROLL_MARGIN).min(top - SCROLL_MARGIN).max(0.0))
        } else {
            None
        };
        operation::Outcome::Some(offset)
    }
}
