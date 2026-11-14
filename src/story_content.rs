//! Turns a story's HTML into what the reader shows: text paragraphs with the
//! story's images in between, in their original order.

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
    /// `RSS_READER_DB=/path/to/rss_reader.db cargo test all_stories -- --ignored`
    #[test]
    #[ignore]
    fn all_stories_convert() {
        let path = std::env::var("RSS_READER_DB").expect("set RSS_READER_DB");
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
