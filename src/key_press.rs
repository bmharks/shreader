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
//! And `find_anchor` / `restore_anchor`, which keep an item where it is on
//! screen while the list around it is reloaded.

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
    /// Offer keys to `on_key` before the children (see `first`)
    first: bool,
    /// Published when the window is asked to close (its close button)
    on_close_request: Option<Message>,
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
        first: false,
        on_close_request: None,
    }
}

impl<'a, Message, Theme, Renderer> OnKeyPress<'a, Message, Theme, Renderer> {
    /// Offer key presses to the mapping function before the children: keys
    /// it maps never reach them (even a focused text field). For shortcuts
    /// that must work while typing, like F3 in a find bar.
    pub fn first(mut self) -> Self {
        self.first = true;
        self
    }

    /// Publish `message` when the window is asked to close. The request
    /// reaches the window's widgets when the window doesn't close itself
    /// (`exit_on_close_request: false`), which lets the app decide.
    pub fn on_close_request(mut self, message: Message) -> Self {
        self.on_close_request = Some(message);
        self
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for OnKeyPress<'a, Message, Theme, Renderer>
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
        let pressed = match &event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some((key.clone(), *modifiers))
            }
            _ => None,
        };
        if let (Event::Window(iced::window::Event::CloseRequested), Some(message)) =
            (&event, &self.on_close_request)
        {
            shell.publish(message.clone());
            return event::Status::Captured;
        }
        if self.first {
            if let Some(message) = pressed.as_ref().and_then(|(key, modifiers)| (self.on_key)(key, *modifiers)) {
                shell.publish(message);
                return event::Status::Captured;
            }
        }

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
    Message: Clone + 'a,
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

/// Scrolls the scrollable `scroll_id` by the distance from container `from`
/// to container `to`, so `to` lands where `from` was: stepping through a
/// list moves the whole list by one item, and the selection stays put on
/// screen (with what's coming next in view below it). Where the list can't
/// scroll that far (near its ends), or `to` would still be out of view, it
/// scrolls just enough to show `to`, like `scroll_into_view`.
pub fn scroll_by_step<Message: Send + 'static>(
    scroll_id: scrollable::Id,
    from: container::Id,
    to: container::Id,
) -> Task<Message> {
    let find = FindPair {
        scroll_id: scroll_id.clone().into(),
        from_id: from.into(),
        to_id: to.into(),
        scroll: None,
        from: None,
        to: None,
    };
    iced::advanced::widget::operate(find).then(move |offset: Option<f32>| match offset {
        Some(y) => scrollable::scroll_to(scroll_id.clone(), scrollable::AbsoluteOffset { x: 0.0, y }),
        None => Task::none(),
    })
}

/// Finds the scrollable and both items; its outcome is the offset to scroll
/// to, if any.
struct FindPair {
    scroll_id: widget::Id,
    from_id: widget::Id,
    to_id: widget::Id,
    /// (viewport bounds, content bounds, current translation)
    scroll: Option<(Rectangle, Rectangle, Vector)>,
    from: Option<Rectangle>,
    to: Option<Rectangle>,
}

impl Operation<Option<f32>> for FindPair {
    fn container(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        operate_on_children: &mut dyn FnMut(&mut dyn Operation<Option<f32>>),
    ) {
        if id == Some(&self.from_id) {
            self.from = Some(bounds);
        }
        if id == Some(&self.to_id) {
            self.to = Some(bounds);
        }
        if self.from.is_none() || self.to.is_none() {
            operate_on_children(self);
        }
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
        let (Some((viewport, content, translation)), Some(to)) = (self.scroll, self.to) else {
            return operation::Outcome::Some(None);
        };
        let max_offset = (content.height - viewport.height).max(0.0);
        // Positions within the scrolled content
        let to_top = to.y - content.y;
        let to_bottom = to_top + to.height;
        let mut offset = match self.from {
            // Move the list by the distance between the two items
            Some(from) => (translation.y + (to.y - from.y)).clamp(0.0, max_offset),
            None => translation.y,
        };
        // Whatever happens, `to` ends up fully in view
        if to_top < offset {
            offset = (to_top - SCROLL_MARGIN).max(0.0);
        } else if to_bottom > offset + viewport.height {
            offset = (to_bottom - viewport.height + SCROLL_MARGIN).min(to_top - SCROLL_MARGIN).max(0.0);
        }
        operation::Outcome::Some((offset != translation.y).then_some(offset))
    }
}

/// Where a list item sits on screen: its id and how far its top is below
/// the top of the scrollable's viewport (negative if partly scrolled past).
pub type Anchor<K> = (K, f32);

/// Finds the item to keep in place while the scrollable `scroll_id` is
/// reloaded: `preferred` if it's in view, otherwise the topmost item in
/// view. `items` names the candidates (container id → key). `None` when
/// none is in view, or when the list is at its very top and `preferred`
/// isn't in view: there, what's new above should show.
pub fn find_anchor<K: Clone + Send + 'static>(
    scroll_id: scrollable::Id,
    items: std::collections::HashMap<widget::Id, K>,
    preferred: Option<container::Id>,
) -> Task<Option<Anchor<K>>> {
    iced::advanced::widget::operate(FindAnchor {
        scroll_id: scroll_id.into(),
        items,
        preferred: preferred.map(Into::into),
        scroll: None,
        found: Vec::new(),
    })
}

struct FindAnchor<K> {
    scroll_id: widget::Id,
    items: std::collections::HashMap<widget::Id, K>,
    preferred: Option<widget::Id>,
    /// (viewport bounds, content bounds, current translation)
    scroll: Option<(Rectangle, Rectangle, Vector)>,
    /// (key, bounds, is the preferred one)
    found: Vec<(K, Rectangle, bool)>,
}

impl<K: Clone + Send + 'static> Operation<Option<Anchor<K>>> for FindAnchor<K> {
    fn container(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        operate_on_children: &mut dyn FnMut(&mut dyn Operation<Option<Anchor<K>>>),
    ) {
        if let Some(key) = id.and_then(|id| self.items.get(id)) {
            let preferred = self.preferred.as_ref() == id;
            self.found.push((key.clone(), bounds, preferred));
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

    fn finish(&self) -> operation::Outcome<Option<Anchor<K>>> {
        let Some((viewport, content, translation)) = self.scroll else {
            return operation::Outcome::Some(None);
        };
        // Top of each item below the viewport's top, and whether it shows
        let on_screen = |bounds: &Rectangle| {
            let from_top = bounds.y - content.y - translation.y;
            (from_top, from_top < viewport.height && from_top + bounds.height > 0.0)
        };
        let visible = || self.found.iter().filter(|(_, b, _)| on_screen(b).1);
        let anchor = visible()
            .find(|(_, _, preferred)| *preferred)
            .or_else(|| (translation.y > 0.0).then(|| visible().min_by(|a, b| a.1.y.total_cmp(&b.1.y))).flatten())
            .map(|(key, bounds, _)| (key.clone(), on_screen(bounds).0));
        operation::Outcome::Some(anchor)
    }
}

/// Scrolls the scrollable `scroll_id` so container `item_id` is
/// `from_top` below the top of its viewport again (as far as the list can
/// scroll). Does nothing if the item is no longer in the list.
pub fn restore_anchor<Message: Send + 'static>(
    scroll_id: scrollable::Id,
    item_id: container::Id,
    from_top: f32,
) -> Task<Message> {
    let find = FindItem {
        scroll_id: scroll_id.clone().into(),
        item_id: item_id.into(),
        scroll: None,
        item: None,
    };
    iced::advanced::widget::operate(PlaceItem { find, from_top }).then(move |offset: Option<f32>| match offset {
        Some(y) => scrollable::scroll_to(scroll_id.clone(), scrollable::AbsoluteOffset { x: 0.0, y }),
        None => Task::none(),
    })
}

/// `FindItem`'s search, with a different outcome: the offset that puts the
/// item `from_top` below the viewport's top.
struct PlaceItem {
    find: FindItem,
    from_top: f32,
}

impl Operation<Option<f32>> for PlaceItem {
    fn container(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        operate_on_children: &mut dyn FnMut(&mut dyn Operation<Option<f32>>),
    ) {
        if self.find.item.is_some() {
            return;
        }
        if id == Some(&self.find.item_id) {
            self.find.item = Some(bounds);
            return;
        }
        operate_on_children(self);
    }

    fn scrollable(
        &mut self,
        state: &mut dyn operation::Scrollable,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
    ) {
        self.find.scrollable(state, id, bounds, content_bounds, translation);
    }

    fn finish(&self) -> operation::Outcome<Option<f32>> {
        let (Some((viewport, content, translation)), Some(item)) = (self.find.scroll, self.find.item) else {
            return operation::Outcome::Some(None);
        };
        let max_offset = (content.height - viewport.height).max(0.0);
        let offset = (item.y - content.y - self.from_top).clamp(0.0, max_offset);
        operation::Outcome::Some(((offset - translation.y).abs() >= 0.5).then_some(offset))
    }
}
