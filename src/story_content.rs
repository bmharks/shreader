//! Turns a story's HTML into what the reader shows: text paragraphs with the
//! story's images in between, in their original order.

use std::borrow::Cow;
use std::collections::HashSet;

use html2text::render::{TaggedLine, TextDecorator};

/// One piece of a story, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    /// An image to download; `src` is an absolute http(s) URL
    Image { src: String },
}

/// Splits `html` into text and image blocks. Relative image URLs are resolved
/// against `base_url` (the story's own link).
pub fn blocks(html: &str, base_url: Option<&str>) -> Vec<Block> {
    let html = &*limit_nesting(html);
    let base = base_url.and_then(|u| url::Url::parse(u).ok());
    let mut blocks = Vec::new();
    let mut seen_images = HashSet::new();
    let mut text_start = 0;
    let mut cursor = 0;

    while let Some((tag_start, tag_end)) = next_img_tag(html, cursor) {
        let tag = &html[tag_start..tag_end];
        cursor = tag_end;

        let Some(src) = image_source(tag, base.as_ref()) else {
            continue; // tracking pixel, data: URI, …: leave it out of the text too
        };
        push_text(&mut blocks, &html[text_start..tag_start]);
        text_start = tag_end;
        // Lazy-loading pages often repeat each image in a <noscript> fallback
        if seen_images.insert(src.clone()) {
            blocks.push(Block::Image { src });
        }
    }
    push_text(&mut blocks, &html[text_start..]);
    blocks
}

fn push_text(blocks: &mut Vec<Block>, html: &str) {
    let text = html_to_text(html);
    if !text.is_empty() {
        blocks.push(Block::Text(text));
    }
}

/// Plain text with each paragraph on one line (the reader wraps it to the
/// column width). Links become just their text: footnote markers like "[3]"
/// would be noise, and would restart in every chunk between images.
pub fn html_to_text(html: &str) -> String {
    // Wide enough that html2text never wraps a paragraph itself
    const NO_WRAP: usize = 100_000;
    html2text::from_read_with_decorator(html.as_bytes(), NO_WRAP, ReaderDecorator)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// Deepest tag nesting converted to text as is. Real pages stay far below
/// this (a few dozen levels); converting gets quadratically slower with
/// depth (100,000 nested `<div>`s, 1 MB of HTML, took 21 seconds).
const MAX_HTML_DEPTH: usize = 256;

/// `html`, unless its tags nest deeper than `MAX_HTML_DEPTH`: then the same
/// text and images with every other tag removed, so converting it stays fast.
/// Formatting is lost only for such (hostile or broken) HTML.
pub fn limit_nesting(html: &str) -> Cow<'_, str> {
    if !nests_deeper_than(html, MAX_HTML_DEPTH) {
        return Cow::Borrowed(html);
    }
    let mut flat = String::with_capacity(html.len() / 2);
    let mut last = 0;
    for (start, end, name, _) in tags(html) {
        flat.push_str(&html[last..start]);
        if name.eq_ignore_ascii_case("img") {
            flat.push_str(&html[start..end]);
        } else {
            flat.push(' ');
        }
        last = end;
    }
    flat.push_str(&html[last..]);
    Cow::Owned(flat)
}

/// Whether open tags nest more than `max` deep. Approximate, but never
/// overcounts real HTML: elements that close themselves or are routinely
/// left unclosed (`<br>`, `<p>`, `<li>`, …) aren't counted.
fn nests_deeper_than(html: &str, max: usize) -> bool {
    const UNCOUNTED: &[&str] = &[
        // Void elements
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
        // End tags often omitted
        "p", "li", "dt", "dd", "tr", "td", "th", "thead", "tbody", "tfoot", "option", "optgroup",
        "colgroup", "rt", "rp", "caption",
    ];
    let mut depth: usize = 0;
    for (_, _, name, kind) in tags(html) {
        if UNCOUNTED.iter().any(|u| name.eq_ignore_ascii_case(u)) {
            continue;
        }
        match kind {
            TagKind::Open => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            TagKind::Close => depth = depth.saturating_sub(1),
            TagKind::SelfClosing => {}
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TagKind {
    Open,
    Close,
    SelfClosing,
}

/// The tags in `html`, as (start, end, name, kind); comments and
/// declarations (`<!…>`, `<?…>`) are skipped. A simple linear scan.
fn tags(html: &str) -> impl Iterator<Item = (usize, usize, &str, TagKind)> {
    let bytes = html.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            let Some(offset) = bytes[i..].iter().position(|&b| b == b'<') else {
                i = bytes.len();
                return None;
            };
            let start = i + offset;
            let rest = &bytes[start..];
            if rest.starts_with(b"<!--") {
                i = match find(rest, b"-->") {
                    Some(end) => start + end + 3,
                    None => bytes.len(),
                };
                continue;
            }
            let closing = rest.get(1) == Some(&b'/');
            let name_start = start + 1 + closing as usize;
            let name_len = bytes[name_start.min(bytes.len())..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric())
                .count();
            if name_len == 0 {
                // "<!DOCTYPE", "<?xml", or a stray "<" in text
                i = start + 1;
                if matches!(rest.get(1), Some(b'!' | b'?')) {
                    i = find(rest, b">").map_or(bytes.len(), |end| start + end + 1);
                }
                continue;
            }
            // The closing '>' that isn't inside a quoted attribute value
            let mut quote = None;
            let mut end = None;
            for (j, &c) in bytes[name_start + name_len..].iter().enumerate() {
                match (quote, c) {
                    (None, b'"' | b'\'') => quote = Some(c),
                    (Some(q), c) if c == q => quote = None,
                    (None, b'>') => {
                        end = Some(name_start + name_len + j + 1);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                i = bytes.len(); // unterminated: the rest is text
                return None;
            };
            i = end;
            let kind = if closing {
                TagKind::Close
            } else if bytes[end - 2] == b'/' {
                TagKind::SelfClosing
            } else {
                TagKind::Open
            };
            return Some((start, end, &html[name_start..name_start + name_len], kind));
        }
        None
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Byte range of the next `<img …>` tag at or after `from`.
fn next_img_tag(html: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = html.as_bytes();
    let mut i = from;
    while i + 4 <= bytes.len() {
        let rest = &bytes[i..];
        if rest[0] == b'<'
            && rest[1..4].eq_ignore_ascii_case(b"img")
            && rest
                .get(4)
                .is_some_and(|c| c.is_ascii_whitespace() || *c == b'/' || *c == b'>')
        {
            // Find the closing '>' that isn't inside a quoted attribute value
            let mut quote = None;
            for (offset, &c) in rest.iter().enumerate().skip(4) {
                match (quote, c) {
                    (None, b'"' | b'\'') => quote = Some(c),
                    (Some(q), c) if c == q => quote = None,
                    (None, b'>') => return Some((i, i + offset + 1)),
                    _ => {}
                }
            }
            return None; // unterminated tag
        }
        i += 1;
    }
    None
}

/// The attributes of an `<img …>` tag, names lowercased.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/');
    let inner = &inner[3.min(inner.len())..]; // skip "img"
    let chars: Vec<char> = inner.chars().collect();
    let mut attrs = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let name_start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' {
            i += 1;
        }
        let name: String = chars[name_start..i].iter().collect::<String>().to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                value = chars[start..i].iter().collect();
                i += 1;
            } else {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                value = chars[start..i].iter().collect();
            }
        }
        if !name.is_empty() {
            attrs.push((name, decode_entities(&value)));
        }
    }
    attrs
}

/// The few entities that commonly appear in URLs inside attributes.
fn decode_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// The URL to download for an `<img>` tag, or `None` if it should be skipped.
fn image_source(tag: &str, base: Option<&url::Url>) -> Option<String> {
    let attrs = attributes(tag);
    let get = |name: &str| {
        attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };

    // Tracking pixels
    let is_tiny = |name: &str| get(name).and_then(|v| v.trim_end_matches("px").parse::<u32>().ok()).is_some_and(|n| n <= 2);
    if is_tiny("width") || is_tiny("height") {
        return None;
    }

    // Lazy-loading pages put a placeholder (often a data: URI) in `src` and the
    // real image in another attribute
    let candidates = [
        get("src"),
        get("data-src"),
        get("data-lazy-src"),
        get("data-original"),
        get("srcset").and_then(best_srcset_candidate),
        get("data-srcset").and_then(best_srcset_candidate),
    ];
    let raw = candidates
        .into_iter()
        .flatten()
        .find(|c| !c.starts_with("data:"))?;

    let resolved = match base {
        Some(base) => base.join(raw).ok()?,
        None => url::Url::parse(raw).ok()?,
    };
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

/// The widest candidate in a `srcset` ("a.jpg 480w, b.jpg 1080w"), or the
/// first one when there are no width descriptors.
fn best_srcset_candidate(srcset: &str) -> Option<&str> {
    srcset
        .split(',')
        .filter_map(|candidate| {
            let mut parts = candidate.split_whitespace();
            let url = parts.next()?;
            let width = parts
                .next()
                .and_then(|d| d.strip_suffix('w'))
                .and_then(|w| w.parse::<u32>().ok())
                .unwrap_or(0);
            Some((url, width))
        })
        .enumerate()
        // Highest width wins; on ties (no descriptors) the first one
        .max_by_key(|(index, (_, width))| (*width, std::cmp::Reverse(*index)))
        .map(|(_, (url, _))| url)
}

/// Plain-text rendering for the reader: like html2text's plain style, but
/// links are just their text (no "[1]" footnotes) and emphasis has no
/// asterisks. Lists, headings and quotes keep their markers.
#[derive(Clone, Copy)]
struct ReaderDecorator;

impl TextDecorator for ReaderDecorator {
    type Annotation = ();

    fn decorate_link_start(&mut self, _url: &str) -> (String, ()) {
        (String::new(), ())
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&self) -> (String, ()) {
        (String::new(), ())
    }
    fn decorate_em_end(&self) -> String {
        String::new()
    }
    fn decorate_strong_start(&self) -> (String, ()) {
        (String::new(), ())
    }
    fn decorate_strong_end(&self) -> String {
        String::new()
    }
    fn decorate_strikeout_start(&self) -> (String, ()) {
        (String::new(), ())
    }
    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }
    fn decorate_code_start(&self) -> (String, ()) {
        (String::new(), ())
    }
    fn decorate_code_end(&self) -> String {
        String::new()
    }
    fn decorate_preformat_first(&self) {}
    fn decorate_preformat_cont(&self) {}
    fn decorate_image(&mut self, _src: &str, _title: &str) -> (String, ()) {
        // Images are shown as real images (or deliberately skipped)
        (String::new(), ())
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
    fn finalise(&mut self, _links: Vec<String>) -> Vec<TaggedLine<()>> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {

    /// Deeply nested HTML is flattened (fast, text and images kept); normal
    /// HTML, including the usual unclosed <p> and <li>, is left alone.
    #[test]
    fn deep_nesting_is_flattened() {
        let deep = format!("{}Hello <img src=\"https://e.com/a.png\"> world{}", "<div>".repeat(100_000), "</div>".repeat(100_000));
        let start = std::time::Instant::now();
        let flat = limit_nesting(&deep);
        assert!(matches!(flat, Cow::Owned(_)));
        assert!(flat.contains("Hello") && flat.contains("<img src=\"https://e.com/a.png\">") && !flat.contains("<div>"));
        let blocks = blocks(&deep, None);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
        assert_eq!(blocks.len(), 3, "{:?}", blocks);

        let normal = format!(
            "<!DOCTYPE html><html><body><div><ul>{}</ul>{}<!-- <div><div> --></div></body></html>",
            "<li>item".repeat(1000),
            "<p>para<br>".repeat(1000)
        );
        assert!(matches!(limit_nesting(&normal), Cow::Borrowed(_)));
    }

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

    #[test]
    fn splits_text_and_images_in_order() {
        let html = r#"<p>Before</p><img src="https://x.com/a.jpg" alt="A"><p>After</p>"#;
        assert_eq!(
            blocks(html, None),
            vec![
                Block::Text("Before".into()),
                Block::Image { src: "https://x.com/a.jpg".into() },
                Block::Text("After".into()),
            ]
        );
    }

    #[test]
    fn resolves_relative_urls_and_entities() {
        let html = r#"<IMG SRC='/img/b.png?w=1&amp;h=2'>"#;
        assert_eq!(
            images(&blocks(html, Some("https://site.org/post/1"))),
            vec!["https://site.org/img/b.png?w=1&h=2"]
        );
    }

    #[test]
    fn prefers_real_source_over_lazy_placeholder() {
        let html = r#"<img src="data:image/gif;base64,R0lGOD" data-src="https://x.com/real.jpg">"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/real.jpg"]);

        let html = r#"<img srcset="https://x.com/s.jpg 480w, https://x.com/l.jpg 1080w">"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/l.jpg"]);
    }

    #[test]
    fn skips_tracking_pixels_and_duplicates() {
        let html = r#"<img src="https://t.co/p.gif" width="1" height="1">
            <img src="https://x.com/a.jpg"><noscript><img src="https://x.com/a.jpg"></noscript>"#;
        assert_eq!(images(&blocks(html, None)), vec!["https://x.com/a.jpg"]);
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
}
