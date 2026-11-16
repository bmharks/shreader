//! Right-click context menu wrapper.
//!
//! Wraps any content. A right click on it publishes `on_right_press`, and
//! remembers where the click happened. When the app then passes a `menu`
//! element (it decides which row's menu is open), the menu is shown as a
//! floating overlay at that point, so it never shifts the layout.
//! Clicking outside the menu publishes `on_dismiss`; Esc publishes `on_escape`
//! (or `on_dismiss` if none is set).

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::keyboard::{self, key};
use iced::{event, mouse, Element, Event, Length, Point, Rectangle, Size, Vector};

pub struct ContextMenu<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    menu: Option<Element<'a, Message, Theme, Renderer>>,
    on_right_press: Option<Message>,
    /// Like `on_right_press`, given where the click landed (relative to the
    /// content's top left)
    on_right_press_at: Option<Box<dyn Fn(Point) -> Message + 'a>>,
    on_left_release: Option<Message>,
    on_dismiss: Option<Message>,
    on_escape: Option<Message>,
    focus_on_right_press: bool,
}

pub fn context_menu<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> ContextMenu<'a, Message, Theme, Renderer> {
    ContextMenu {
        content: content.into(),
        menu: None,
        on_right_press: None,
        on_right_press_at: None,
        on_left_release: None,
        on_dismiss: None,
        on_escape: None,
        focus_on_right_press: false,
    }
}

impl<'a, Message, Theme, Renderer> ContextMenu<'a, Message, Theme, Renderer> {
    /// The menu to show. Pass `Some` only while this wrapper's menu is open.
    pub fn menu(mut self, menu: Option<impl Into<Element<'a, Message, Theme, Renderer>>>) -> Self {
        self.menu = menu.map(Into::into);
        self
    }

    pub fn on_right_press(mut self, message: Message) -> Self {
        self.on_right_press = Some(message);
        self
    }

    /// Like `on_right_press`, but the message gets the click's position
    /// relative to the content's top left.
    pub fn on_right_press_at(mut self, message: impl Fn(Point) -> Message + 'a) -> Self {
        self.on_right_press_at = Some(Box::new(message));
        self
    }

    /// Published when the left button is released over the content, even if
    /// the content handled the click itself (as a text editor does).
    pub fn on_left_release(mut self, message: Message) -> Self {
        self.on_left_release = Some(message);
        self
    }

    pub fn on_dismiss(mut self, message: Message) -> Self {
        self.on_dismiss = Some(message);
        self
    }

    /// Message for Esc while the menu is open (e.g. "go back a page").
    /// Defaults to `on_dismiss`.
    pub fn on_escape(mut self, message: Message) -> Self {
        self.on_escape = Some(message);
        self
    }

    /// Give keyboard focus to the content on right click, as a left click
    /// would. Needed for a read-only `text_editor`: it only shows its
    /// selection (and handles Ctrl+C) while focused, and "Select all" from
    /// the menu should be visible and copyable.
    pub fn focus_on_right_press(mut self) -> Self {
        self.focus_on_right_press = true;
        self
    }
}

#[derive(Default)]
struct State {
    /// Where the last right click landed, in this widget's coordinate space
    position: Option<Point>,
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for ContextMenu<'a, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        let mut children = vec![Tree::new(&self.content)];
        if let Some(menu) = &self.menu {
            children.push(Tree::new(menu));
        }
        children
    }

    fn diff(&self, tree: &mut Tree) {
        match &self.menu {
            Some(menu) => tree.diff_children(&[&self.content, menu]),
            None => tree.diff_children(std::slice::from_ref(&self.content)),
        }
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
        let status = self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event.clone(),
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if let (Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)), Some(message)) =
            (&event, &self.on_left_release)
        {
            if cursor.is_over(layout.bounds()) {
                shell.publish(message.clone());
            }
        }
        if let event::Status::Captured = status {
            return event::Status::Captured;
        }

        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event {
            let bounds = layout.bounds();
            if let Some(position) = cursor.position_over(bounds) {
                let message = match &self.on_right_press_at {
                    Some(at) => Some(at(Point::new(position.x - bounds.x, position.y - bounds.y))),
                    None => self.on_right_press.clone(),
                };
                if let Some(message) = message {
                    tree.state.downcast_mut::<State>().position = Some(position);
                    if self.focus_on_right_press {
                        self.content.as_widget().operate(
                            &mut tree.children[0],
                            layout,
                            renderer,
                            &mut FocusAll,
                        );
                    }
                    shell.publish(message);
                    return event::Status::Captured;
                }
            }
        }

        event::Status::Ignored
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
        let position = tree.state.downcast_ref::<State>().position;

        match (&mut self.menu, position) {
            (Some(menu), Some(position)) if tree.children.len() > 1 => {
                Some(overlay::Element::new(Box::new(MenuOverlay {
                    menu,
                    tree: &mut tree.children[1],
                    // `translation` converts from this widget's (possibly scrolled)
                    // coordinates to window coordinates
                    position: position + translation,
                    on_dismiss: self.on_dismiss.clone(),
                    on_escape: self.on_escape.clone().or_else(|| self.on_dismiss.clone()),
                })))
            }
            _ => self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                translation,
            ),
        }
    }
}

/// Focuses every focusable widget it visits.
struct FocusAll;

impl widget::Operation for FocusAll {
    fn container(
        &mut self,
        _id: Option<&widget::Id>,
        _bounds: Rectangle,
        operate_on_children: &mut dyn FnMut(&mut dyn widget::Operation),
    ) {
        operate_on_children(self);
    }

    fn focusable(&mut self, state: &mut dyn widget::operation::Focusable, _id: Option<&widget::Id>) {
        state.focus();
    }
}

struct MenuOverlay<'a, 'b, Message, Theme, Renderer> {
    menu: &'a mut Element<'b, Message, Theme, Renderer>,
    tree: &'a mut Tree,
    position: Point,
    on_dismiss: Option<Message>,
    on_escape: Option<Message>,
}

impl<'a, 'b, Message, Theme, Renderer> overlay::Overlay<Message, Theme, Renderer>
    for MenuOverlay<'a, 'b, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: iced::advanced::Renderer,
{
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.menu.as_widget().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = node.size();

        // Keep the menu inside the window: shift left at the right edge, and
        // open upwards when there isn't room below the cursor.
        let x = self.position.x.min(bounds.width - size.width).max(0.0);
        let y = if self.position.y + size.height > bounds.height {
            (self.position.y - size.height).max(0.0)
        } else {
            self.position.y
        };

        node.move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &layout.bounds(),
        );
    }

    fn on_event(
        &mut self,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> event::Status {
        let status = self.menu.as_widget_mut().on_event(
            self.tree,
            event.clone(),
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
        if status == event::Status::Captured {
            return status;
        }

        // Esc is handled here rather than by an app-level key subscription:
        // overlays see events before the widgets underneath, so a focused text
        // field (e.g. the search box) can't swallow the Esc first.
        if let Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key::Named::Escape),
            ..
        }) = event
        {
            if let Some(message) = &self.on_escape {
                shell.publish(message.clone());
                return event::Status::Captured;
            }
        }

        let Event::Mouse(mouse_event) = event else {
            return event::Status::Ignored;
        };

        let over_menu = cursor.is_over(layout.bounds());
        if let mouse::Event::ButtonPressed(_) = mouse_event {
            if !over_menu {
                // Click elsewhere closes the menu. The click itself still reaches
                // the UI underneath (e.g. selecting another feed works in one click).
                if let Some(message) = &self.on_dismiss {
                    shell.publish(message.clone());
                }
                return event::Status::Ignored;
            }
        }

        // Swallow mouse events over the menu so they don't hit the rows beneath it
        if over_menu {
            event::Status::Captured
        } else {
            event::Status::Ignored
        }
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.menu
            .as_widget()
            .mouse_interaction(self.tree, layout, cursor, viewport, renderer)
    }
}

impl<'a, Message, Theme, Renderer> From<ContextMenu<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: ContextMenu<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
