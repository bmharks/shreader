//! An icon that spins, to show work in progress.
//!
//! The angle comes from the frame timestamp at draw time and the widget asks
//! for the next frame itself, so the animation never produces app messages:
//! `update`/`view` don't run 60 times a second while a sync is going.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::svg::{Renderer as SvgRenderer, Svg};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::time::Instant;
use iced::{event, mouse, window, Element, Event, Length, Radians, Rectangle, Size, Theme};

use crate::icons::{handle, Icon, Tint};

/// Seconds per full turn
const PERIOD: f32 = 1.0;

/// Time between animation frames (60 per second)
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_micros(16_667);

pub struct Spinner {
    icon: Icon,
    size: f32,
    tint: Tint,
}

/// `icon`, `size` pixels square, rotating once a second.
pub fn spinner(icon: Icon, size: f32, tint: Tint) -> Spinner {
    Spinner { icon, size, tint }
}

#[derive(Default)]
struct State {
    started: Option<Instant>,
    rotation: f32,
}

impl<Message, Renderer> Widget<Message, Theme, Renderer> for Spinner
where
    Renderer: iced::advanced::Renderer + SvgRenderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(self.size, self.size, Size::new(self.size, self.size)))
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<State>();
            let started = *state.started.get_or_insert(now);
            let turns = now.duration_since(started).as_secs_f32() / PERIOD;
            state.rotation = turns.fract() * std::f32::consts::TAU;
            // At most 60 frames a second, rather than every frame the display
            // takes: with mailbox presentation (see main.rs) nothing else
            // limits the rate, and the spinner alone kept a CPU core busy
            shell.request_redraw(window::RedrawRequest::At(now + FRAME_INTERVAL));
        }
        event::Status::Ignored
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let rotation = tree.state.downcast_ref::<State>().rotation;
        renderer.draw_svg(
            Svg {
                handle: handle(self.icon),
                color: Some(self.tint.color(theme)),
                rotation: Radians(rotation),
                opacity: 1.0,
            },
            layout.bounds(),
        );
    }
}

impl<'a, Message, Renderer> From<Spinner> for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer + SvgRenderer + 'a,
{
    fn from(spinner: Spinner) -> Self {
        Element::new(spinner)
    }
}
