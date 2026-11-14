use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::db::Database;

pub struct OpmlOutline {
    pub title: String,
    pub xml_url: String,
    pub html_url: Option<String>,
    pub folder: Option<String>,
}

/// Parses an OPML XML string and extracts all feed URLs, titles, and their parent folder names.
pub fn parse_opml(opml_content: &str) -> Result<Vec<OpmlOutline>> {
    let mut reader = Reader::from_str(opml_content);
    reader.trim_text(true);

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
                        let value = String::from_utf8_lossy(&attr.value).to_string();

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
                        let display_title = title
                            .or(text_attr)
                            .unwrap_or_else(|| url.clone());

                        outlines.push(OpmlOutline {
                            title: display_title,
                            xml_url: url,
                            html_url,
                            folder: folder_stack.last().cloned(),
                        });
                        tag_stack.push(false);
                    } else {
                        // Outline without xmlUrl is a folder group (Akregator / OPML 2.0 standard)
                        let folder_name = title
                            .or(text_attr)
                            .unwrap_or_else(|| "Uncategorized".to_string());
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
                        let value = String::from_utf8_lossy(&attr.value).to_string();

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
                        let display_title = title
                            .or(text_attr)
                            .unwrap_or_else(|| url.clone());

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

/// Exports all subscribed feeds from the database to an OPML 2.0 XML string with folder outlines.
pub fn export_opml_from_db(db: &Database) -> Result<String> {
    let folders = db.get_folders().context("Failed to get folders for OPML export")?;
    let feeds = db.get_feeds().context("Failed to get feeds for OPML export")?;

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<opml version=\"2.0\">\n");
    out.push_str("  <head>\n");
    out.push_str("    <title>RSS Reader Subscriptions</title>\n");
    out.push_str("  </head>\n");
    out.push_str("  <body>\n");

    // 1. Export folders with their nested feeds (Akregator style)
    for folder in &folders {
        let safe_folder_name = escape_xml(&folder.name);
        out.push_str(&format!(
            "    <outline text=\"{}\" title=\"{}\">\n",
            safe_folder_name, safe_folder_name
        ));

        for feed in feeds.iter().filter(|f| f.folder_id == Some(folder.id)) {
            let safe_title = escape_xml(&feed.title);
            let safe_xml_url = escape_xml(&feed.url);
            let safe_html_url = feed.site_url.as_deref().map(escape_xml).unwrap_or_default();

            out.push_str(&format!(
                "      <outline text=\"{}\" title=\"{}\" type=\"rss\" xmlUrl=\"{}\" htmlUrl=\"{}\" />\n",
                safe_title, safe_title, safe_xml_url, safe_html_url
            ));
        }

        out.push_str("    </outline>\n");
    }

    // 2. Export uncategorized / root feeds
    for feed in feeds.iter().filter(|f| f.folder_id.is_none()) {
        let safe_title = escape_xml(&feed.title);
        let safe_xml_url = escape_xml(&feed.url);
        let safe_html_url = feed.site_url.as_deref().map(escape_xml).unwrap_or_default();

        out.push_str(&format!(
            "    <outline text=\"{}\" title=\"{}\" type=\"rss\" xmlUrl=\"{}\" htmlUrl=\"{}\" />\n",
            safe_title, safe_title, safe_xml_url, safe_html_url
        ));
    }

    out.push_str("  </body>\n");
    out.push_str("</opml>\n");

    Ok(out)
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
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
