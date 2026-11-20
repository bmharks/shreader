use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::db::Database;
use crate::feed::{clean_line, MAX_TITLE_CHARS};

pub struct OpmlOutline {
    pub title: String,
    pub xml_url: String,
    pub html_url: Option<String>,
    pub folder: Option<String>,
}

/// Longest folder name taken from an OPML file. Feed and folder names are
/// cleaned like names from feeds (`clean_line`): OPML files get shared, and
/// a name could carry terminal escape codes (printed by `--list-feeds`) or
/// be large enough to slow the sidebar.
const MAX_FOLDER_NAME_CHARS: usize = 200;

/// Parses an OPML XML string and extracts all feed URLs, titles, and their parent folder names.
pub fn parse_opml(opml_content: &str) -> Result<Vec<OpmlOutline>> {
    let mut reader = Reader::from_str(opml_content);
    reader.config_mut().trim_text(true);

    let mut outlines = Vec::new();
    let mut folder_stack: Vec<String> = Vec::new();
    let mut tag_stack: Vec<bool> = Vec::new(); // true if start tag was a folder

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.name().as_ref() == b"outline" {
                    let mut xml_url = None;
                    let mut title = None;
                    let mut text_attr = None;
                    let mut html_url = None;

                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        // Decode entities: a URL like "?a=1&amp;b=2" must become "?a=1&b=2"
                        let value = attr
                            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                            .map(|v| v.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).to_string());

                        if key.eq_ignore_ascii_case(b"xmlurl") {
                            xml_url = Some(value);
                        } else if key.eq_ignore_ascii_case(b"title") {
                            title = Some(value);
                        } else if key.eq_ignore_ascii_case(b"text") {
                            text_attr = Some(value);
                        } else if key.eq_ignore_ascii_case(b"htmlurl") {
                            html_url = Some(value);
                        }
                    }

                    if let Some(url) = xml_url {
                        let display_title = clean_line(&title.or(text_attr).unwrap_or_else(|| url.clone()), MAX_TITLE_CHARS);

                        outlines.push(OpmlOutline {
                            title: display_title,
                            xml_url: url,
                            html_url,
                            folder: folder_stack.last().cloned(),
                        });
                        tag_stack.push(false);
                    } else {
                        // Outline without xmlUrl is a folder group (Akregator / OPML 2.0 standard)
                        let folder_name = clean_line(
                            &title.or(text_attr).unwrap_or_else(|| "Uncategorized".to_string()),
                            MAX_FOLDER_NAME_CHARS,
                        );
                        folder_stack.push(folder_name);
                        tag_stack.push(true);
                    }
                }
            }
            Event::Empty(ref e) => {
                if e.name().as_ref() == b"outline" {
                    let mut xml_url = None;
                    let mut title = None;
                    let mut text_attr = None;
                    let mut html_url = None;

                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        // Decode entities: a URL like "?a=1&amp;b=2" must become "?a=1&b=2"
                        let value = attr
                            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                            .map(|v| v.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).to_string());

                        if key.eq_ignore_ascii_case(b"xmlurl") {
                            xml_url = Some(value);
                        } else if key.eq_ignore_ascii_case(b"title") {
                            title = Some(value);
                        } else if key.eq_ignore_ascii_case(b"text") {
                            text_attr = Some(value);
                        } else if key.eq_ignore_ascii_case(b"htmlurl") {
                            html_url = Some(value);
                        }
                    }

                    if let Some(url) = xml_url {
                        let display_title = clean_line(&title.or(text_attr).unwrap_or_else(|| url.clone()), MAX_TITLE_CHARS);

                        outlines.push(OpmlOutline {
                            title: display_title,
                            xml_url: url,
                            html_url,
                            folder: folder_stack.last().cloned(),
                        });
                    }
                }
            }
            Event::End(ref e) => {
                if e.name().as_ref() == b"outline" {
                    if let Some(was_folder) = tag_stack.pop() {
                        if was_folder {
                            folder_stack.pop();
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(outlines)
}

/// Imports feeds from an OPML file directly into the database, preserving folder groupings.
pub fn import_opml_into_db(opml_content: &str, db: &Database) -> Result<usize> {
    let outlines = parse_opml(opml_content)?;
    let mut count = 0;

    for outline in outlines {
        if !outline.xml_url.trim().is_empty() {
            let folder_id = if let Some(ref folder_name) = outline.folder {
                if !folder_name.trim().is_empty() {
                    Some(db.get_or_create_folder(folder_name)?)
                } else {
                    None
                }
            } else {
                None
            };

            db.add_or_update_feed(
                &outline.xml_url,
                &outline.title,
                None,
                outline.html_url.as_deref(),
                folder_id,
            )?;
            count += 1;
        }
    }

    Ok(count)
}

/// What an OPML import changed.
#[derive(Debug, Clone, Default)]
pub struct OpmlImport {
    /// Ids of the feeds that were added (to fetch their stories)
    pub added: Vec<i64>,
    /// Feeds skipped because they were already subscribed (or listed twice)
    pub skipped: usize,
    pub folders_created: usize,
}

/// Imports feeds from OPML, leaving feeds that are already subscribed alone
/// (their names and folders stay as the user set them). Folders are matched
/// by name, case-insensitively, and created when missing.
pub fn import_new_feeds(opml_content: &str, db: &Database) -> Result<OpmlImport> {
    let outlines = parse_opml(opml_content)?;
    let mut known_urls: std::collections::HashSet<String> =
        db.get_feeds()?.into_iter().map(|f| f.url).collect();
    let folders_before = db.get_folders()?.len();
    let mut result = OpmlImport::default();

    for outline in outlines {
        let url = outline.xml_url.trim().to_string();
        if url.is_empty() {
            continue;
        }
        if !known_urls.insert(url.clone()) {
            result.skipped += 1;
            continue;
        }
        let folder_id = match outline.folder.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => Some(db.get_or_create_folder(name)?),
            _ => None,
        };
        let title = outline.title.trim();
        let id = db.add_or_update_feed(
            &url,
            if title.is_empty() { &url } else { title },
            None,
            outline.html_url.as_deref(),
            folder_id,
        )?;
        result.added.push(id);
    }

    result.folders_created = db.get_folders()?.len().saturating_sub(folders_before);
    Ok(result)
}

/// Exports all subscribed feeds from the database to an OPML 2.0 XML string with folder outlines.
pub fn export_opml_from_db(db: &Database) -> Result<String> {
    use quick_xml::events::{BytesDecl, BytesText, Event};

    let folders = db.get_folders().context("Failed to get folders for OPML export")?;
    let feeds = db.get_feeds().context("Failed to get feeds for OPML export")?;

    // quick-xml escapes the text and attribute values
    let mut writer = quick_xml::Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.create_element("opml").with_attribute(("version", "2.0")).write_inner_content(|w| {
        w.create_element("head").write_inner_content(|w| {
            w.create_element("title")
                .write_text_content(BytesText::new("Shreader RSS Reader Subscriptions"))?;
            Ok(())
        })?;
        w.create_element("body").write_inner_content(|w| {
            // 1. Folders with their feeds inside (Akregator style)
            for folder in &folders {
                w.create_element("outline")
                    .with_attributes([("text", folder.name.as_str()), ("title", folder.name.as_str())])
                    .write_inner_content(|w| {
                        for feed in feeds.iter().filter(|f| f.folder_id == Some(folder.id)) {
                            write_feed_outline(w, feed)?;
                        }
                        Ok(())
                    })?;
            }
            // 2. Feeds in no folder
            for feed in feeds.iter().filter(|f| f.folder_id.is_none()) {
                write_feed_outline(w, feed)?;
            }
            Ok(())
        })?;
        Ok(())
    })?;

    let mut out = String::from_utf8(writer.into_inner()).context("OPML export isn't UTF-8")?;
    out.push('\n');
    Ok(out)
}

/// One feed's `<outline>`. `htmlUrl` is left out when unknown rather than
/// written empty.
fn write_feed_outline<W: std::io::Write>(
    writer: &mut quick_xml::Writer<W>,
    feed: &crate::models::Feed,
) -> std::io::Result<()> {
    let mut outline = writer.create_element("outline").with_attributes([
        ("text", feed.title.as_str()),
        ("title", feed.title.as_str()),
        ("type", "rss"),
        ("xmlUrl", feed.url.as_str()),
    ]);
    if let Some(site) = feed.site_url.as_deref().filter(|s| !s.is_empty()) {
        outline = outline.with_attribute(("htmlUrl", site));
    }
    outline.write_empty()?;
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Names from a shared OPML file are cleaned and capped like feed names.
    #[test]
    fn imported_names_are_cleaned() {
        let long = "x".repeat(10_000);
        let opml = format!(
            r#"<opml version="2.0"><body><outline text="Evil&#27;[2J folder {long}"><outline text="Feed&#27;]0;pwned&#7;" xmlUrl="https://e.com/rss"/></outline></body></opml>"#
        );
        let outlines = parse_opml(&opml).unwrap();
        assert_eq!(outlines.len(), 1);
        assert!(!outlines[0].title.chars().any(char::is_control), "{:?}", outlines[0].title);
        let folder = outlines[0].folder.as_deref().unwrap();
        assert!(!folder.chars().any(char::is_control));
        assert_eq!(folder.chars().count(), MAX_FOLDER_NAME_CHARS + 1); // plus "…"
    }

    use super::*;

    #[test]
    fn test_opml_parsing_with_folders() {
        let opml = r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <opml version="2.0">
            <head><title>My Feeds</title></head>
            <body>
                <outline text="Technology" title="Technology">
                    <outline text="Rust Blog" title="Rust Blog" type="rss" 
                        xmlUrl="https://blog.rust-lang.org/feed.xml" htmlUrl="https://blog.rust-lang.org" />
                    <outline text="Phoronix" title="Phoronix" type="rss" 
                        xmlUrl="https://www.phoronix.com/rss.php" htmlUrl="https://www.phoronix.com" />
                </outline>
                <outline text="Hacker News" title="Hacker News" type="rss" 
                    xmlUrl="https://news.ycombinator.com/rss" htmlUrl="https://news.ycombinator.com" />
            </body>
        </opml>
        "#;

        let db = Database::open_in_memory().unwrap();
        let imported = import_opml_into_db(opml, &db).unwrap();
        assert_eq!(imported, 3);

        let folders = db.get_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Technology");
        assert_eq!(folders[0].feed_count, 2);

        let feeds = db.get_feeds().unwrap();
        assert_eq!(feeds.len(), 3);

        let rust_blog = feeds.iter().find(|f| f.title == "Rust Blog").unwrap();
        assert_eq!(rust_blog.folder_id, Some(folders[0].id));

        let hn = feeds.iter().find(|f| f.title == "Hacker News").unwrap();
        assert_eq!(hn.folder_id, None);

        let exported = export_opml_from_db(&db).unwrap();
        assert!(exported.contains("<outline text=\"Technology\""));
        assert!(exported.contains("https://blog.rust-lang.org/feed.xml"));
        assert!(exported.contains("https://news.ycombinator.com/rss"));
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;

    #[test]
    fn imports_new_feeds_and_skips_existing() {
        let db = Database::open_in_memory().unwrap();
        let existing = db
            .add_or_update_feed("https://a.com/feed", "My Renamed A", None, None, None)
            .unwrap();
        let opml = r#"<?xml version="1.0"?>
            <opml version="2.0"><body>
              <outline text="Tech">
                <outline text="A" xmlUrl="https://a.com/feed"/>
                <outline text="B" xmlUrl="https://b.com/rss?x=1&amp;y=2"/>
              </outline>
              <outline text="C" xmlUrl="https://c.com/atom"/>
              <outline text="C again" xmlUrl="https://c.com/atom"/>
            </body></opml>"#;

        let result = import_new_feeds(opml, &db).unwrap();
        assert_eq!(result.added.len(), 2);
        assert_eq!(result.skipped, 2);
        assert_eq!(result.folders_created, 1);

        let feeds = db.get_feeds().unwrap();
        // Existing feed untouched (name, no folder)
        let a = feeds.iter().find(|f| f.id == existing).unwrap();
        assert_eq!(a.title, "My Renamed A");
        assert_eq!(a.folder_id, None);
        // Entities in URLs decoded; folder applied
        let b = feeds.iter().find(|f| f.title == "B").unwrap();
        assert_eq!(b.url, "https://b.com/rss?x=1&y=2");
        assert!(b.folder_id.is_some());
    }

    #[test]
    fn export_round_trips_through_import() {
        let db = Database::open_in_memory().unwrap();
        let folder = db.create_folder("News & Views").unwrap();
        let feed = db
            .add_or_update_feed("https://a.com/feed?x=1&y=2", "A <Blog>", None, None, None)
            .unwrap();
        db.set_feed_folder(feed, Some(folder)).unwrap();
        db.add_or_update_feed("https://b.com/rss", "B \"quoted\"", None, None, None)
            .unwrap();

        let exported = export_opml_from_db(&db).unwrap();
        assert!(!exported.contains("htmlUrl=\"\""));
        assert!(exported.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(exported.contains(r#"text="A &lt;Blog&gt;""#), "{}", exported);
        assert!(exported.contains(r#"xmlUrl="https://a.com/feed?x=1&amp;y=2""#), "{}", exported);

        let copy = Database::open_in_memory().unwrap();
        let import = import_new_feeds(&exported, &copy).unwrap();
        assert_eq!(import.added.len(), 2);
        let feeds = copy.get_feeds().unwrap();
        let a = feeds.iter().find(|f| f.url == "https://a.com/feed?x=1&y=2").unwrap();
        assert_eq!(a.title, "A <Blog>");
        let folders = copy.get_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "News & Views");
        assert_eq!(a.folder_id, Some(folders[0].id));
        assert!(feeds.iter().any(|f| f.title == "B \"quoted\""));
    }
}
