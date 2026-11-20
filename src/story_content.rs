//! Turns a story's HTML into what the reader shows: text paragraphs with the
//! story's images in between, in their original order.
//!
//! The HTML is parsed once, by html5ever (the HTML standard's parser, via
//! scraper), as a browser without scripts would. Images are chosen from the
//! parsed elements and replaced by numbered markers; html2text then lays out
//! the whole story in one go and reports each marker where it falls, which
//! is where the text is split around the image.

use std::collections::HashSet;

use ego_tree::NodeId;
use html2text::render::{TaggedLine, TextDecorator};
use html5ever::{local_name, namespace_url, ns, Attribute, QualName};
use scraper::node::Element;
use scraper::{Html, Node};

/// One piece of a story, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Text, one paragraph per line, and the links in it
    Text { text: String, links: Vec<LinkSpan> },
    /// An image to download; `src` is an absolute http(s) URL
    Image { src: String },
}

/// A link in a text block: bytes `start..end` of line `line` lead to `url`
/// (absolute; http, https or mailto).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpan {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Splits `html` into text and image blocks. Relative links and image URLs
/// are resolved against `base_url` (the story's own link).
pub fn blocks(html: &str, base_url: Option<&str>) -> Vec<Block> {
    let base = base_url.and_then(|u| url::Url::parse(u).ok());
    let mut doc = parse(html);
    let images = mark_images(&mut doc, base.as_ref());

    // Wide enough that html2text never wraps a paragraph itself
    const NO_WRAP: usize = 100_000;
    let Ok(lines) =
        html2text::config::with_decorator(ReaderDecorator).lines_from_read(doc.html().as_bytes(), NO_WRAP)
    else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    let mut text = TextBlock::default();
    for line in &lines {
        let mut current = String::new();
        // Just after an image: the space that was beside it is left out
        let mut after_image = false;
        for piece in line.tagged_strings() {
            let marks = piece.tag.iter().flatten();
            if let Some(&Mark::Image(n)) = marks.clone().find(|m| matches!(m, Mark::Image(_))) {
                // The text so far ends a block, the image follows, and the
                // rest of the line starts the next block
                current.truncate(current.trim_end().len());
                let line_number = text.lines.len();
                for link in text.links.iter_mut().filter(|l| l.line == line_number) {
                    link.end = link.end.min(current.len());
                }
                text.links.retain(|l| l.start < l.end);
                text.lines.push(std::mem::take(&mut current));
                text.finish(&mut blocks);
                blocks.push(Block::Image { src: images[n].clone() });
                after_image = true;
                continue;
            }
            let piece_text = if after_image { piece.s.trim_start() } else { piece.s.as_str() };
            if piece_text.is_empty() {
                continue;
            }
            after_image = false;
            let start = current.len();
            current.push_str(piece_text);
            // The innermost link, if the piece is inside one
            let link = marks.rev().find_map(|m| match m {
                Mark::Link(url) => Some(url),
                Mark::Image(_) => None,
            });
            let Some(url) = link.and_then(|url| resolve_link(url, base.as_ref())) else {
                continue;
            };
            let line_number = text.lines.len();
            match text.links.last_mut() {
                // A link split into pieces (e.g. by <em> inside it)
                Some(last) if last.line == line_number && last.end == start && last.url == url => {
                    last.end = current.len();
                }
                _ => text.links.push(LinkSpan { line: line_number, start, end: current.len(), url }),
            }
        }
        text.lines.push(current);
    }
    text.finish(&mut blocks);
    blocks
}

/// Plain text of a story, wrapped at `width` columns (for search). Parsed
/// like `blocks`; images show as html2text's usual "[alt text]".
pub fn plain_text(html: &str, width: usize) -> Option<String> {
    let mut doc = parse(html);
    // Its images are left as they are, but the noscript copies of them
    // would otherwise show as markup
    unwrap_noscript(&mut doc);
    html2text::from_read(doc.html().as_bytes(), width).ok()
}

/// A text block being collected: its lines, and the links in them.
#[derive(Default)]
struct TextBlock {
    lines: Vec<String>,
    links: Vec<LinkSpan>,
}

impl TextBlock {
    /// Adds the block to `blocks`, without blank lines at its start and end
    /// (or not at all, if that leaves nothing), and starts a new one.
    fn finish(&mut self, blocks: &mut Vec<Block>) {
        let TextBlock { lines, links } = std::mem::take(self);
        let first = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
        let last = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(first, |i| i + 1);
        if first == last {
            return;
        }
        let links = links
            .into_iter()
            .filter(|link| (first..last).contains(&link.line))
            .map(|link| LinkSpan { line: link.line - first, ..link })
            .collect();
        blocks.push(Block::Text { text: lines[first..last].join("\n"), links });
    }
}

/// A link's address made absolute, if it's one worth offering: web pages
/// (relative ones resolved against the story's address) and email.
fn resolve_link(href: &str, base: Option<&url::Url>) -> Option<String> {
    let href = href.trim();
    let url = match base {
        Some(base) => base.join(href).ok()?,
        None => url::Url::parse(href).ok()?,
    };
    matches!(url.scheme(), "http" | "https" | "mailto").then(|| url.to_string())
}

/// Deepest element nesting laid out as is. Real pages stay far below this
/// (a few dozen levels). Deeper HTML is slow to handle: html5ever's tree
/// builder, like a browser's, takes quadratic time on it (100,000 nested
/// `<div>`s, 1 MB of HTML, took over 100 seconds), and so does html2text.
const MAX_HTML_DEPTH: usize = 256;

/// Parses a story as a browser without scripts would: `<noscript>`
/// content is real content (lazy-loading pages put their images there).
/// HTML nested deeper than `MAX_HTML_DEPTH` is flattened instead.
fn parse(html: &str) -> Html {
    use html5ever::driver::{parse_document, ParseOpts};
    use html5ever::tendril::TendrilSink;
    use html5ever::tree_builder::TreeBuilderOpts;
    let scan = scan(html);
    if scan.too_deep.get() {
        return flat_document(scan.pieces.into_inner());
    }
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
        ..Default::default()
    };
    parse_document(scraper::HtmlTreeSink::new(Html::new_document()), opts).one(html)
}

/// Elements not counted toward nesting depth: void elements, and ones whose
/// end tag is routinely left out (counting those would overcount real HTML)
const UNCOUNTED: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "p", "li", "dt", "dd", "tr", "td", "th", "thead", "tbody", "tfoot", "option",
    "optgroup", "colgroup", "rt", "rp", "caption",
];

/// Runs html5ever's tokenizer (linear time, unlike tree building) over
/// `html`: how deep its tags nest, roughly, and its text and images in
/// reading order, to flatten it with if that's too deep.
fn scan(html: &str) -> Scan {
    use html5ever::tokenizer::{BufferQueue, Tokenizer, TokenizerOpts};
    let input = BufferQueue::default();
    input.push_back(html.into());
    let tokenizer = Tokenizer::new(Scan::default(), TokenizerOpts::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink
}

#[derive(Default)]
struct Scan {
    depth: std::cell::Cell<usize>,
    too_deep: std::cell::Cell<bool>,
    /// Inside an element whose content isn't shown (`<script>`, `<style>`)
    hidden: std::cell::Cell<bool>,
    pieces: std::cell::RefCell<Vec<Piece>>,
}

/// Text or an image, as the tokenizer found it
enum Piece {
    Text(scraper::StrTendril),
    Image(Vec<Attribute>),
}

impl html5ever::tokenizer::TokenSink for Scan {
    type Handle = ();

    fn process_token(
        &self,
        token: html5ever::tokenizer::Token,
        _line: u64,
    ) -> html5ever::tokenizer::TokenSinkResult<()> {
        use html5ever::tokenizer::states::RawKind;
        use html5ever::tokenizer::{TagKind, Token, TokenSinkResult};
        let mut pieces = self.pieces.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                let name: &str = &tag.name;
                let counted = !UNCOUNTED.contains(&name);
                // A tag separates the text on either side of it
                if matches!(pieces.last(), Some(Piece::Text(t)) if !t.ends_with(char::is_whitespace)) {
                    pieces.push(Piece::Text(" ".into()));
                }
                match tag.kind {
                    TagKind::StartTag => {
                        if counted && !tag.self_closing {
                            self.depth.set(self.depth.get() + 1);
                            if self.depth.get() > MAX_HTML_DEPTH {
                                self.too_deep.set(true);
                            }
                        }
                        if name == "img" && !self.hidden.get() {
                            pieces.push(Piece::Image(tag.attrs));
                            return TokenSinkResult::Continue;
                        }
                        // As the tree builder tells the tokenizer: these
                        // elements' content is text, not markup
                        let (raw, hidden) = match name {
                            "script" => (RawKind::ScriptData, true),
                            "style" | "xmp" | "iframe" | "noembed" | "noframes" => (RawKind::Rawtext, true),
                            "title" => (RawKind::Rcdata, true),
                            "textarea" => (RawKind::Rcdata, false),
                            "plaintext" => return TokenSinkResult::Plaintext,
                            _ => return TokenSinkResult::Continue,
                        };
                        self.hidden.set(hidden);
                        return TokenSinkResult::RawData(raw);
                    }
                    TagKind::EndTag => {
                        if counted {
                            self.depth.set(self.depth.get().saturating_sub(1));
                        }
                        // In raw text, the only end tag is the element's own
                        self.hidden.set(false);
                    }
                }
            }
            Token::CharacterTokens(text) if !self.hidden.get() => pieces.push(Piece::Text(text)),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// A document of just `pieces` in `<body>`: the text and images of HTML
/// too deeply nested to lay out, without the elements around them.
fn flat_document(pieces: Vec<Piece>) -> Html {
    let element =
        |name, attrs| Node::Element(Element::new(QualName::new(None, ns!(html), name), attrs));
    let mut doc = Html::new_document();
    let mut root = doc.tree.root_mut();
    let mut html = root.append(element(local_name!("html"), Vec::new()));
    let mut body = html.append(element(local_name!("body"), Vec::new()));
    for piece in pieces {
        body.append(match piece {
            Piece::Text(text) => Node::Text(scraper::node::Text { text }),
            Piece::Image(attrs) => element(local_name!("img"), attrs),
        });
    }
    doc
}

/// The elements named `name`, in document order.
fn element_ids(doc: &Html, name: &str) -> Vec<NodeId> {
    doc.tree
        .root()
        .descendants()
        .filter(|n| matches!(n.value(), Node::Element(e) if e.name() == name))
        .map(|n| n.id())
        .collect()
}

/// Replaces each `<noscript>` with its content, as a browser without
/// scripts shows it. (html2text parses as a browser with scripts, which
/// would show the content as markup.)
fn unwrap_noscript(doc: &mut Html) {
    for noscript in element_ids(doc, "noscript") {
        let children: Vec<NodeId> =
            doc.tree.get(noscript).map(|n| n.children().map(|c| c.id()).collect()).unwrap_or_default();
        let Some(mut node) = doc.tree.get_mut(noscript) else { continue };
        if node.parent().is_none() {
            continue; // already moved out of the document
        }
        for child in children {
            node.insert_id_before(child);
        }
        node.detach();
    }
}

/// `src` of the image markers `mark_images` leaves for html2text
const IMAGE_MARKER: &str = "shreader-image:";

/// Chooses each image's address and replaces the image with a marker
/// (`<img src="shreader-image:N">`, N indexing the returned addresses).
/// Images not worth showing (tracking pixels, repeats, ones without a
/// usable address) are removed, from the text too.
fn mark_images(doc: &mut Html, base: Option<&url::Url>) -> Vec<String> {
    unwrap_noscript(doc);
    let mut sources: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for id in element_ids(doc, "img") {
        let chosen = doc.tree.get(id).and_then(|img| image_source(img, base));
        let Some(mut node) = doc.tree.get_mut(id) else { continue };
        match chosen {
            // Lazy-loading pages often repeat each image in a <noscript> fallback
            Some(src) if seen.insert(src.clone()) => {
                let attr = |name, value: &str| Attribute {
                    name: QualName::new(None, ns!(), name),
                    value: value.into(),
                };
                let marker = format!("{}{}", IMAGE_MARKER, sources.len());
                // html2text only reports images with alt text
                *node.value() = Node::Element(Element::new(
                    QualName::new(None, ns!(html), local_name!("img")),
                    vec![attr(local_name!("src"), &marker), attr(local_name!("alt"), "image")],
                ));
                sources.push(src);
            }
            _ => node.detach(),
        }
    }
    sources
}

/// Image types a `<picture>` source can be shown in
const SHOWN_TYPES: &[&str] =
    &["image/jpeg", "image/png", "image/gif", "image/webp", "image/bmp", "image/svg+xml"];

/// The address to download for an `<img>`, or `None` if it should be skipped.
fn image_source(img: ego_tree::NodeRef<Node>, base: Option<&url::Url>) -> Option<String> {
    let element = img.value().as_element()?;
    let get = |name: &str| element.attr(name).map(str::trim).filter(|v| !v.is_empty());

    // Tracking pixels
    let is_tiny =
        |name: &str| get(name).and_then(|v| v.trim_end_matches("px").parse::<u32>().ok()).is_some_and(|n| n <= 2);
    if is_tiny("width") || is_tiny("height") {
        return None;
    }

    // In a <picture>, the first <source> the browser would pick: one for any
    // screen (a `media` query is for some screens only) in a type that shows
    let from_picture = img
        .parent()
        .filter(|p| matches!(p.value(), Node::Element(e) if e.name() == "picture"))
        .into_iter()
        .flat_map(|picture| picture.children())
        .take_while(|child| child.id() != img.id())
        .filter_map(|child| child.value().as_element().filter(|e| e.name() == "source"))
        .find(|source| {
            source.attr("media").is_none_or(|m| m.trim().is_empty() || m.trim().eq_ignore_ascii_case("all"))
                && source.attr("type").is_none_or(|t| SHOWN_TYPES.contains(&t.trim().to_ascii_lowercase().as_str()))
        })
        .and_then(|source| source.attr("srcset"))
        .and_then(best_srcset_candidate);

    // Lazy-loading pages put a placeholder (often a data: URI) in `src` and
    // the real image in another attribute
    let candidates = [
        from_picture,
        get("src"),
        get("data-src"),
        get("data-lazy-src"),
        get("data-original"),
        get("srcset").and_then(best_srcset_candidate),
        get("data-srcset").and_then(best_srcset_candidate),
    ];
    let raw = candidates.into_iter().flatten().find(|c| !c.starts_with("data:"))?;
    let resolved = match base {
        Some(base) => base.join(raw).ok()?,
        None => url::Url::parse(raw).ok()?,
    };
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

/// The best candidate in a `srcset`: the widest ("480w"), else the densest
/// ("2x"), else the first. Parsed as the HTML standard says, so a URL may
/// contain commas ("w_400,h_300/a.jpg 400w") and a candidate with an
/// invalid descriptor is dropped.
fn best_srcset_candidate(srcset: &str) -> Option<&str> {
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r');
    let mut candidates: Vec<(&str, f64, f64)> = Vec::new(); // (url, width, density)
    let mut rest = srcset;
    loop {
        rest = rest.trim_start_matches(|c: char| is_space(c) || c == ',');
        if rest.is_empty() {
            break;
        }
        let url_end = rest.find(is_space).unwrap_or(rest.len());
        let (mut url, after) = rest.split_at(url_end);
        rest = after;
        let mut descriptors: Vec<String> = Vec::new();
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            // Descriptors, up to a comma outside parentheses
            let mut current = String::new();
            let mut in_parens = false;
            let mut used = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    ',' if !in_parens => {
                        used = i + 1;
                        break;
                    }
                    '(' => in_parens = true,
                    ')' => in_parens = false,
                    c if is_space(c) && !in_parens => {
                        if !current.is_empty() {
                            descriptors.push(std::mem::take(&mut current));
                        }
                        continue;
                    }
                    _ => {}
                }
                current.push(c);
            }
            if !current.is_empty() {
                descriptors.push(current);
            }
            rest = &rest[used..];
        }
        let (mut width, mut density, mut valid) = (None, None, true);
        for d in &descriptors {
            let (number, unit) = d.split_at(d.len() - 1);
            match unit {
                "w" if width.is_none() && density.is_none() => {
                    width = number.parse::<u32>().ok().filter(|&w| w > 0).map(f64::from);
                    valid &= width.is_some();
                }
                "x" if width.is_none() && density.is_none() => {
                    density = number.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0);
                    valid &= density.is_some();
                }
                "h" => {} // only meaningful with a width; not used for choosing
                _ => valid = false,
            }
        }
        if valid && !url.is_empty() {
            candidates.push((url, width.unwrap_or(0.0), density.unwrap_or(1.0)));
        }
    }
    candidates
        .iter()
        .enumerate()
        // Highest width, then density; on ties the first one
        .max_by(|(i, a), (j, b)| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)).then(j.cmp(i)))
        .map(|(_, (url, _, _))| *url)
}

/// What html2text attaches to a piece of text
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mark {
    /// Inside a link to this address
    Link(String),
    /// The marker of image N (see `mark_images`)
    Image(usize),
}

/// Plain-text rendering for the reader: like html2text's plain style, but
/// links are just their text (no "[1]" footnotes) and emphasis has no
/// asterisks. Lists, headings and quotes keep their markers.
#[derive(Clone, Copy)]
struct ReaderDecorator;

impl TextDecorator for ReaderDecorator {
    type Annotation = Option<Mark>;

    fn decorate_link_start(&mut self, url: &str) -> (String, Option<Mark>) {
        (String::new(), Some(Mark::Link(url.to_string())))
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&self) -> (String, Option<Mark>) {
        (String::new(), None)
    }
    fn decorate_em_end(&self) -> String {
        String::new()
    }
    fn decorate_strong_start(&self) -> (String, Option<Mark>) {
        (String::new(), None)
    }
    fn decorate_strong_end(&self) -> String {
        String::new()
    }
    fn decorate_strikeout_start(&self) -> (String, Option<Mark>) {
        (String::new(), None)
    }
    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }
    fn decorate_code_start(&self) -> (String, Option<Mark>) {
        (String::new(), None)
    }
    fn decorate_code_end(&self) -> String {
        String::new()
    }
    fn decorate_preformat_first(&self) -> Option<Mark> {
        None
    }
    fn decorate_preformat_cont(&self) -> Option<Mark> {
        None
    }
    fn decorate_image(&mut self, src: &str, _title: &str) -> (String, Option<Mark>) {
        // Where `blocks` puts the image; U+FFFC is Unicode's placeholder for
        // an embedded object (never shown: the text is split there)
        match src.strip_prefix(IMAGE_MARKER).and_then(|n| n.parse().ok()) {
            Some(n) => ("\u{FFFC}".to_string(), Some(Mark::Image(n))),
            None => (String::new(), None),
        }
    }
    fn header_prefix(&self, _level: usize) -> String {
        String::new()
    }
    // All prefixes must be plain ASCII: html2text estimates them by display
    // width but lays out by byte length, and panics when the two differ
    // ("│ " or "• " are 2 columns but 4 bytes): an assertion on <blockquote>,
    // an integer underflow on an empty <li>.
    fn quote_prefix(&self) -> String {
        "> ".to_string()
    }
    fn unordered_item_prefix(&self) -> String {
        "- ".to_string()
    }
    fn ordered_item_prefix(&self, i: i64) -> String {
        format!("{}. ", i)
    }
    fn make_subblock_decorator(&self) -> Self {
        *self
    }
    fn finalise(&mut self, _links: Vec<String>) -> Vec<TaggedLine<Option<Mark>>> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn images(blocks: &[Block]) -> Vec<&str> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Image { src } => Some(src.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The story's text alone: its text blocks, a blank line apart
    fn html_to_text(html: &str) -> String {
        blocks(html, None)
            .into_iter()
            .filter_map(|b| match b {
                Block::Text { text, .. } => Some(text),
                Block::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Deeply nested HTML is flattened (fast, text and images kept); normal
    /// HTML, including the usual unclosed <p> and <li>, is left alone.
    #[test]
    fn deep_nesting_is_flattened() {
        let deep = format!(
            "{}Hello <img src=\"https://e.com/a.png\"> world{}",
            "<div>".repeat(100_000),
            "</div>".repeat(100_000)
        );
        let start = std::time::Instant::now();
        let blocks = blocks(&deep, None);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
        assert_eq!(
            blocks,
            [
                Block::Text { text: "Hello".into(), links: vec![] },
                Block::Image { src: "https://e.com/a.png".into() },
                Block::Text { text: "world".into(), links: vec![] },
            ]
        );
        assert!(plain_text(&deep, 80).unwrap().contains("Hello"));

        let normal = format!(
            "<!DOCTYPE html><html><body><div><ul>{}</ul>{}<!-- <div><div> --></div></body></html>",
            "<li>item".repeat(1000),
            "<p>para<br>".repeat(1000)
        );
        assert!(!scan(&normal).too_deep.get());
    }

    #[test]
    fn splits_text_and_images_in_order() {
        let html = r#"<p>Before</p><img src="https://x.com/a.jpg" alt="A"><p>After</p>"#;
        assert_eq!(
            blocks(html, None),
            vec![
                Block::Text { text: "Before".into(), links: vec![] },
                Block::Image { src: "https://x.com/a.jpg".into() },
                Block::Text { text: "After".into(), links: vec![] },
            ]
        );
    }

    /// Text around an image keeps its structure: the list goes on after it.
    #[test]
    fn an_image_inside_a_list_keeps_the_list() {
        let html = r#"<ul><li>One</li><li>Two <img src="https://x.com/a.jpg"> more</li><li>Three</li></ul>"#;
        let blocks = blocks(html, None);
        assert_eq!(images(&blocks), ["https://x.com/a.jpg"]);
        assert_eq!(blocks[0], Block::Text { text: "- One\n- Two".into(), links: vec![] });
        assert_eq!(blocks[2], Block::Text { text: "more\n- Three".into(), links: vec![] });
    }

    /// Attribute values are decoded as the HTML standard says, so
    /// WordPress's "&#038;" in image addresses is a "&".
    #[test]
    fn resolves_relative_urls_and_entities() {
        let html = r#"<IMG SRC='/img/b.png?w=1&amp;h=2'><img src="https://x.com/c.jpg?w=1&#038;h=2&#x26;q=3">"#;
        assert_eq!(
            images(&blocks(html, Some("https://site.org/post/1"))),
            ["https://site.org/img/b.png?w=1&h=2", "https://x.com/c.jpg?w=1&h=2&q=3"]
        );
    }

    #[test]
    fn prefers_real_source_over_lazy_placeholder() {
        let html = r#"<img src="data:image/gif;base64,R0lGOD" data-src="https://x.com/real.jpg">"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/real.jpg"]);

        let html = r#"<img srcset="https://x.com/s.jpg 480w, https://x.com/l.jpg 1080w">"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/l.jpg"]);
    }

    /// `srcset` is parsed as the standard says: commas inside a URL,
    /// densities, and candidates with descriptors that aren't valid.
    #[test]
    fn srcset_follows_the_standard() {
        assert_eq!(
            best_srcset_candidate("https://c.example/w_400,h_300/a.jpg 400w, https://c.example/w_800,h_600/a.jpg 800w"),
            Some("https://c.example/w_800,h_600/a.jpg")
        );
        assert_eq!(best_srcset_candidate("a.jpg, b.jpg 2x, c.jpg 1.5x"), Some("b.jpg"));
        assert_eq!(best_srcset_candidate("a.jpg 100w, b.jpg 9000q, c.jpg 50w"), Some("a.jpg"));
        // A comma straight after a URL ends it; one inside it is part of it
        assert_eq!(best_srcset_candidate("first.jpg, second.jpg 2x"), Some("second.jpg"));
        assert_eq!(best_srcset_candidate("first.jpg,second.jpg"), Some("first.jpg,second.jpg"));
        assert_eq!(best_srcset_candidate("  ,, "), None);
    }

    #[test]
    fn skips_tracking_pixels_and_duplicates() {
        let html = r#"<img src="https://t.co/p.gif" width="1" height="1">
            <img src="https://x.com/a.jpg"><noscript><img src="https://x.com/a.jpg"></noscript>"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/a.jpg"]);
    }

    /// Only images a browser would show: not ones in comments or scripts.
    /// Without scripts, <noscript> content is shown, as text, not markup.
    #[test]
    fn hidden_images_stay_hidden() {
        let html = r#"<p>Text</p><!-- <img src="https://x.com/old.jpg"> -->
            <script>var s = '<img src="https://x.com/js.jpg">';</script>
            <noscript><p>Fallback</p><img src="https://x.com/only.jpg"></noscript>"#;
        let blocks = blocks(html, None);
        assert_eq!(images(&blocks), ["https://x.com/only.jpg"]);
        assert_eq!(html_to_text(html), "Text\n\nFallback");
        assert!(!plain_text(html, 80).unwrap().contains('<'));
    }

    /// In a <picture>, the source a browser would pick: for any screen, in
    /// a type that can be shown; otherwise the <img> itself.
    #[test]
    fn pictures_use_the_source_a_browser_would() {
        let picture = |sources: &str| {
            let html = format!(r#"<picture>{}<img src="https://x.com/fallback.jpg"></picture>"#, sources);
            images(&blocks(&html, None)).iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(
            picture(r#"<source srcset="https://x.com/a.avif" type="image/avif"><source srcset="https://x.com/a.webp 1x, https://x.com/b.webp 2x" type="image/webp">"#),
            ["https://x.com/b.webp"]
        );
        assert_eq!(
            picture(r#"<source media="(max-width: 600px)" srcset="https://x.com/small.jpg">"#),
            ["https://x.com/fallback.jpg"]
        );
    }

    #[test]
    fn quotes_headings_and_empty_items_dont_panic() {
        assert_eq!(html_to_text("<ul><li></li><li>Item</li></ul>"), "- Item");
        let text = html_to_text("<h2>Title</h2><blockquote><p>Quoted</p><blockquote>Nested</blockquote></blockquote>");
        assert!(text.contains("> Quoted"));
        assert!(text.contains("> > Nested"));
    }

    /// Converts every story in a database without panicking:
    /// `SHREADER_DB=/path/to/shreader.db cargo test all_stories -- --ignored`
    #[test]
    #[ignore]
    fn all_stories_convert() {
        let path = std::env::var("SHREADER_DB").expect("set SHREADER_DB");
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn
            .prepare("SELECT id, coalesce(content_html, summary, ''), url FROM articles")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))
            .unwrap();
        let mut count = 0;
        for row in rows {
            let (id, html, url) = row.unwrap();
            let result = std::panic::catch_unwind(|| blocks(&html, url.as_deref()));
            assert!(result.is_ok(), "story {} panicked", id);
            count += 1;
        }
        eprintln!("converted {} stories", count);
    }

    #[test]
    fn links_have_no_footnotes() {
        let text = html_to_text(r#"<p>See <a href="https://x.com">this</a>.</p><ul><li>One</li></ul>"#);
        assert_eq!(text, "See this.\n\n- One");
    }

    /// Links are found with their position in the text, relative addresses
    /// resolved; ones that aren't web or email links are plain text. Line
    /// numbers count from the start of each block, around images too.
    #[test]
    fn links_are_located() {
        let html = r#"<p>Read <a href="/2026/11/post">the <em>full</em> post</a> or
            <a href="https://other.example/x?a=1&amp;b=2">this</a>.</p>
            <p>Mail <a href="mailto:me@example.com">me</a>, not <a href="javascript:alert(1)">this</a>.</p>
            <img src="https://x.com/a.jpg"><p>After <a href="https://after.example/">it</a></p>"#;
        let mut found = Vec::new();
        for block in blocks(html, Some("https://pluralistic.net/2026/11/15/")) {
            if let Block::Text { text, links } = block {
                let lines: Vec<&str> = text.lines().collect();
                found.extend(links.iter().map(|l| (lines[l.line][l.start..l.end].to_string(), l.url.clone())));
            }
        }
        let found: Vec<(&str, &str)> = found.iter().map(|(t, u)| (t.as_str(), u.as_str())).collect();
        assert_eq!(
            found,
            [
                ("the full post", "https://pluralistic.net/2026/11/post"),
                ("this", "https://other.example/x?a=1&b=2"),
                ("me", "mailto:me@example.com"),
                ("it", "https://after.example/"),
            ]
        );
    }
}
