//! Extras drawn around a block of story text: the pointing-hand cursor over
//! links, and find-in-story highlights.
//!
//! The story text is a read-only `text_editor` (so it can be selected), and
//! the editor can neither say what's under the mouse nor draw backgrounds
//! behind parts of its text. So this wrapper lays out the same text a second
//! time, cut into pieces at link and match boundaries, with exactly the
//! editor's font, size, line height and wrapping (hence the same line
//! breaks). That layout says which piece the mouse is over, and where each
//! match sits; the highlights are drawn beneath the editor's (transparent)
//! background.
//!
//! The layout is made only when needed (the mouse is over the block, or it
//! has matches), and remade only when the width or the matches change.

use std::cell::RefCell;

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{
    alignment, event, mouse, Background, Border, Color, Element, Event, Length, Pixels, Rectangle, Size, Vector,
};

use crate::story_content::LinkSpan;

/// A find match: bytes `start..end` of line `line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

const MATCH_COLOR: Color = Color::from_rgba(1.0, 0.85, 0.2, 0.35);
const CURRENT_MATCH_COLOR: Color = Color::from_rgba(1.0, 0.55, 0.1, 0.75);

pub struct StoryText<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    text: &'a str,
    links: &'a [LinkSpan],
    matches: &'a [TextRange],
    /// Which of `matches` is the current one, if it's in this block
    current: Option<usize>,
    /// Reported (via `operate`) as the bounds of the current match, so the
    /// reader can scroll to it
    current_id: widget::Id,
    size: Pixels,
    wrapping: text::Wrapping,
}

/// Wraps `content`, a text editor showing `text` at `size` with `wrapping`.
pub fn story_text<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    text: &'a str,
    links: &'a [LinkSpan],
    size: impl Into<Pixels>,
    wrapping: text::Wrapping,
) -> StoryText<'a, Message, Theme, Renderer> {
    StoryText {
        content: content.into(),
        text,
        links,
        matches: &[],
        current: None,
        current_id: widget::Id::unique(),
        size: size.into(),
        wrapping,
    }
}

impl<'a, Message, Theme, Renderer> StoryText<'a, Message, Theme, Renderer> {
    /// Highlights `matches`; `current` (an index into them) more strongly,
    /// and reports its bounds as a container with `current_id`.
    pub fn matches(mut self, matches: &'a [TextRange], current: Option<usize>, current_id: widget::Id) -> Self {
        self.matches = matches;
        self.current = current;
        self.current_id = current_id;
        self
    }
}

/// What a piece of the second layout is
#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    link: bool,
    /// The match it's part of (an index into `matches`)
    match_index: Option<usize>,
}

/// The second layout, and what it was made for
struct Cache<P> {
    width: f32,
    matches: Vec<TextRange>,
    paragraph: P,
    pieces: Vec<Piece>,
    /// Each match's rectangles (one per line it's on), relative to the block
    match_bounds: Vec<Vec<Rectangle>>,
}

struct State<P> {
    cache: RefCell<Option<Cache<P>>>,
}

impl<Message, Theme, Renderer> StoryText<'_, Message, Theme, Renderer>
where
    Renderer: text::Renderer,
{
    /// The text cut at every link and match boundary, and what each piece is
    fn pieces(&self) -> (Vec<text::Span<'_, (), Renderer::Font>>, Vec<Piece>) {
        let mut spans = Vec::new();
        let mut pieces = Vec::new();
        for (number, line) in self.text.split('\n').enumerate() {
            if number > 0 {
                spans.push(text::Span::new("\n"));
                pieces.push(Piece { link: false, match_index: None });
            }
            let mut cuts = vec![0, line.len()];
            let links: Vec<&LinkSpan> = self.links.iter().filter(|l| l.line == number && l.end <= line.len()).collect();
            let matches: Vec<(usize, &TextRange)> = self
                .matches
                .iter()
                .enumerate()
                .filter(|(_, m)| m.line == number && m.end <= line.len())
                .collect();
            cuts.extend(links.iter().flat_map(|l| [l.start, l.end]));
            cuts.extend(matches.iter().flat_map(|(_, m)| [m.start, m.end]));
            cuts.retain(|&cut| line.is_char_boundary(cut));
            cuts.sort_unstable();
            cuts.dedup();
            for pair in cuts.windows(2) {
                let (start, end) = (pair[0], pair[1]);
                spans.push(text::Span::new(&line[start..end]));
                pieces.push(Piece {
                    link: links.iter().any(|l| l.start <= start && end <= l.end),
                    match_index: matches.iter().find(|(_, m)| m.start <= start && end <= m.end).map(|(i, _)| *i),
                });
            }
        }
        (spans, pieces)
    }

    /// The second layout for `width`, made or remade if needed.
    fn with_cache<T>(
        &self,
        state: &State<Renderer::Paragraph>,
        renderer: &Renderer,
        width: f32,
        f: impl FnOnce(&Cache<Renderer::Paragraph>) -> T,
    ) -> T {
        let mut cache = state.cache.borrow_mut();
        let fresh = cache.as_ref().is_some_and(|c| c.width == width && c.matches == self.matches);
        if !fresh {
            let (spans, pieces) = self.pieces();
            let paragraph = Renderer::Paragraph::with_spans(text::Text {
                content: spans.as_slice(),
                bounds: Size::new(width, f32::INFINITY),
                size: self.size,
                line_height: text::LineHeight::default(),
                font: renderer.default_font(),
                horizontal_alignment: alignment::Horizontal::Left,
                vertical_alignment: alignment::Vertical::Top,
                shaping: text::Shaping::Advanced,
                wrapping: self.wrapping,
            });
            let mut match_bounds = vec![Vec::new(); self.matches.len()];
            for (index, piece) in pieces.iter().enumerate() {
                if let Some(m) = piece.match_index {
                    match_bounds[m].extend(paragraph.span_bounds(index));
                }
            }
            *cache = Some(Cache { width, matches: self.matches.to_vec(), paragraph, pieces, match_bounds });
        }
        f(cache.as_ref().expect("just made"))
    }

    /// The current match's bounds, in window coordinates
    fn current_bounds(&self, state: &State<Renderer::Paragraph>, renderer: &Renderer, bounds: Rectangle) -> Option<Rectangle> {
        let current = self.current?;
        let rects = self.with_cache(state, renderer, bounds.width, |cache| cache.match_bounds.get(current).cloned())?;
        let union = rects.into_iter().reduce(|a, b| a.union(&b))?;
        Some(union + Vector::new(bounds.x, bounds.y))
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for StoryText<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph> { cache: RefCell::new(None) })
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
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        if let Some(bounds) = self.current_bounds(state, renderer, layout.bounds()) {
            operation.container(Some(&self.current_id), bounds, &mut |_| {});
        }
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
                let on_link = self.with_cache(state, renderer, bounds.width, |cache| {
                    cache
                        .paragraph
                        .hit_span(position)
                        .and_then(|index| cache.pieces.get(index))
                        .is_some_and(|piece| piece.link)
                });
                if on_link {
                    return mouse::Interaction::Pointer;
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
        let bounds = layout.bounds();
        if !self.matches.is_empty() && bounds.intersects(viewport) {
            let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
            // Copied out: drawing needs the renderer mutably
            let rects: Vec<(Rectangle, bool)> = self.with_cache(state, renderer, bounds.width, |cache| {
                cache
                    .match_bounds
                    .iter()
                    .enumerate()
                    .flat_map(|(index, rects)| rects.iter().map(move |r| (*r, Some(index) == self.current)))
                    .map(|(r, current)| (r + Vector::new(bounds.x, bounds.y), current))
                    .filter(|(r, _)| r.intersects(viewport))
                    .collect()
            });
            for (rect, current) in rects {
                renderer.fill_quad(
                    Quad {
                        bounds: rect.expand(1.0),
                        border: Border { radius: 2.0.into(), ..Border::default() },
                        ..Quad::default()
                    },
                    Background::Color(if current { CURRENT_MATCH_COLOR } else { MATCH_COLOR }),
                );
            }
        }
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

impl<'a, Message, Theme, Renderer> From<StoryText<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: text::Renderer + 'static,
{
    fn from(widget: StoryText<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

/// Every occurrence of `query` in `text`, line by line, ignoring case and
/// accents as the story list's search does ("cafe" finds "Café"). At most
/// `limit`.
pub fn find_matches(text: &str, query: &str, limit: usize) -> Vec<TextRange> {
    let needle: Vec<char> = query.chars().flat_map(fold).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (number, line) in text.split('\n').enumerate() {
        // The line folded, each character remembering which bytes of the
        // original it came from (folding can change lengths)
        let mut folded: Vec<(char, usize, usize)> = Vec::new();
        for (start, c) in line.char_indices() {
            let end = start + c.len_utf8();
            let mut any = false;
            for f in fold(c) {
                folded.push((f, start, end));
                any = true;
            }
            // A combining accent written on its own belongs to the letter
            // before it, so a match on that letter includes it
            if !any {
                if let Some(last) = folded.last_mut() {
                    last.2 = end;
                }
            }
        }
        let mut i = 0;
        while i + needle.len() <= folded.len() {
            if folded[i..i + needle.len()].iter().map(|(c, _, _)| *c).eq(needle.iter().copied()) {
                found.push(TextRange { line: number, start: folded[i].1, end: folded[i + needle.len() - 1].2 });
                if found.len() >= limit {
                    return found;
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    found
}

/// `c` without its accents (its Unicode canonical decomposition, NFD,
/// without the combining marks), lowercased.
fn fold(c: char) -> impl Iterator<Item = char> {
    use unicode_normalization::char::is_combining_mark;
    use unicode_normalization::UnicodeNormalization;
    std::iter::once(c).nfd().filter(|d| !is_combining_mark(*d)).flat_map(char::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_match_ignoring_case() {
        let text = "The cat and the Hat.\nTHE END, then the end";
        let found = find_matches(text, "the", 100);
        let words: Vec<&str> = found
            .iter()
            .map(|m| &text.split('\n').nth(m.line).unwrap()[m.start..m.end])
            .collect();
        assert_eq!(words, ["The", "the", "THE", "the", "the"]);
        // Phrases, non-ASCII, and the limit
        assert_eq!(find_matches("Ärger über Äpfel", "äpfel", 10).len(), 1);
        assert_eq!(find_matches("aaaa", "a", 3).len(), 3);
        assert!(find_matches("anything", "", 10).is_empty());
        assert_eq!(find_matches(text, "the end", 10).len(), 2);
    }

    /// Accents are ignored either way round, as in the story list's search,
    /// and a match covers its letters' accents, however they're written.
    #[test]
    fn finds_matches_ignoring_accents() {
        let text = "Café, CAFÉ and cafe\u{301}; naïve résumé";
        let found = find_matches(text, "cafe", 10);
        let words: Vec<&str> = found.iter().map(|m| &text[m.start..m.end]).collect();
        assert_eq!(words, ["Café", "CAFÉ", "cafe\u{301}"]);
        assert_eq!(find_matches(text, "café", 10).len(), 3);
        assert_eq!(find_matches(text, "naive resume", 10).len(), 1);
        assert_eq!(find_matches("Ångström", "angstrom", 10).len(), 1);
    }
}
