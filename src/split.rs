//! Two panes side by side with a draggable divider between them.
//!
//! The left pane gets `left_width` (clamped so both panes keep a minimum
//! width), the right pane fills the rest. Dragging the divider publishes
//! `on_resize` with the new width, and `on_resize_end` once the button is
//! released (a good moment to save it).
//!
//! The clamping happens in `layout`, against the space the split actually
//! has, so shrinking the window never squeezes the right pane to nothing,
//! and the app doesn't need to track the window size.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{event, mouse, Element, Event, Length, Point, Rectangle, Size, Theme, Vector};

/// How far from the divider line (on each side) a press still grabs it.
const GRAB_MARGIN: f32 = 4.0;
/// Width of the visible divider line.
const LINE_WIDTH: f32 = 1.0;

pub struct Split<'a, Message, Renderer> {
    children: [Element<'a, Message, Theme, Renderer>; 2],
    left_width: f32,
    min_left: f32,
    min_right: f32,
    on_resize: Box<dyn Fn(f32) -> Message + 'a>,
    on_resize_end: Option<Message>,
}

pub fn split<'a, Message, Renderer>(
    left: impl Into<Element<'a, Message, Theme, Renderer>>,
    right: impl Into<Element<'a, Message, Theme, Renderer>>,
    left_width: f32,
    on_resize: impl Fn(f32) -> Message + 'a,
) -> Split<'a, Message, Renderer> {
    Split {
        children: [left.into(), right.into()],
        left_width,
        min_left: 0.0,
        min_right: 0.0,
        on_resize: Box::new(on_resize),
        on_resize_end: None,
    }
}

impl<'a, Message, Renderer> Split<'a, Message, Renderer> {
    /// Minimum widths of the left and right panes.
    pub fn min_widths(mut self, left: f32, right: f32) -> Self {
        self.min_left = left;
        self.min_right = right;
        self
    }

    /// Published when a drag of the divider ends.
    pub fn on_resize_end(mut self, message: Message) -> Self {
        self.on_resize_end = Some(message);
        self
    }

    /// The left pane width that fits in `total_width`.
    fn clamp_left(&self, width: f32, total_width: f32) -> f32 {
        // If both minimums don't fit, the left pane gives way first
        let max = (total_width - LINE_WIDTH - self.min_right).max(0.0);
        width.max(self.min_left).min(max).round()
    }
}

#[derive(Default)]
struct State {
    /// Set while the divider is being dragged: distance from the cursor to the
    /// line when it was grabbed, so the line doesn't jump to the cursor.
    drag_offset: Option<f32>,
    /// Last width published, to skip messages for sub-pixel moves
    last_published: Option<f32>,
    /// Cursor is over the grab area (drawn highlighted)
    hovered: bool,
}

/// The divider's grab area, from the left pane's layout.
fn handle_bounds(layout: Layout<'_>) -> Rectangle {
    let bounds = layout.bounds();
    let line_x = layout
        .children()
        .next()
        .map(|left| left.bounds().x + left.bounds().width)
        .unwrap_or(bounds.x);
    Rectangle {
        x: line_x - GRAB_MARGIN,
        y: bounds.y,
        width: LINE_WIDTH + 2.0 * GRAB_MARGIN,
        height: bounds.height,
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for Split<'a, Message, Renderer>
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
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let total = limits.max();
        let left_width = self.clamp_left(self.left_width, total.width);
        let right_width = (total.width - left_width - LINE_WIDTH).max(0.0);

        let left = self.children[0].as_widget().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(left_width, total.height))
                .width(left_width)
                .height(total.height),
        );
        let right = self.children[1]
            .as_widget()
            .layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(right_width, total.height))
                    .width(right_width)
                    .height(total.height),
            )
            .move_to(Point::new(left_width + LINE_WIDTH, 0.0));

        layout::Node::with_children(total, vec![left, right])
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        for ((child, state), layout) in self
            .children
            .iter()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget().operate(state, layout, renderer, operation);
        }
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
        let bounds = layout.bounds();
        let handle = handle_bounds(layout);
        let state = tree.state.downcast_mut::<State>();

        // Redraw when the hover highlight should appear or disappear
        if let Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) = event {
            let hovered = cursor.is_over(handle);
            if hovered != state.hovered {
                state.hovered = hovered;
                shell.request_redraw(iced::window::RedrawRequest::NextFrame);
            }
        }

        match (&event, state.drag_offset) {
            // Grab the divider. Checked before the panes see the press.
            (Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)), None) => {
                if let Some(position) = cursor.position_over(handle) {
                    let line_x = handle.x + GRAB_MARGIN;
                    state.drag_offset = Some(position.x - line_x);
                    state.last_published = None;
                    return event::Status::Captured;
                }
            }
            (Event::Mouse(mouse::Event::CursorMoved { position }), Some(offset)) => {
                let width = self.clamp_left(position.x - offset - bounds.x, bounds.width);
                if state.last_published != Some(width) {
                    state.last_published = Some(width);
                    shell.publish((self.on_resize)(width));
                }
                return event::Status::Captured;
            }
            (Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)), Some(_)) => {
                state.drag_offset = None;
                if let Some(message) = &self.on_resize_end {
                    shell.publish(message.clone());
                }
                return event::Status::Captured;
            }
            // While dragging, keep other mouse input (hover, scroll) away from the panes
            (Event::Mouse(_), Some(_)) => return event::Status::Captured,
            _ => {}
        }

        self.children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .map(|((child, state), layout)| {
                child.as_widget_mut().on_event(
                    state,
                    event.clone(),
                    layout,
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                )
            })
            .fold(event::Status::Ignored, event::Status::merge)
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        if state.drag_offset.is_some() || cursor.is_over(handle_bounds(layout)) {
            return mouse::Interaction::ResizingHorizontally;
        }
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(state, layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
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
        for ((child, state), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child
                .as_widget()
                .draw(state, renderer, theme, style, layout, cursor, viewport);
        }

        // The divider: a thin rule normally, a wider accent bar while hovered or
        // dragged, so it's clear it can be moved
        let handle = handle_bounds(layout);
        let palette = theme.extended_palette();
        let active = tree.state.downcast_ref::<State>().drag_offset.is_some()
            || cursor.is_over(handle);
        let (width, color) = if active {
            (3.0, palette.primary.base.color)
        } else {
            (LINE_WIDTH, palette.background.strong.color)
        };
        let line_x = handle.x + GRAB_MARGIN + LINE_WIDTH / 2.0;
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x: line_x - width / 2.0,
                    y: handle.y,
                    width,
                    height: handle.height,
                },
                ..renderer::Quad::default()
            },
            color,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(&mut self.children, tree, layout, renderer, translation)
    }
}

impl<'a, Message, Renderer> From<Split<'a, Message, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: Split<'a, Message, Renderer>) -> Self {
        Element::new(widget)
    }
}
