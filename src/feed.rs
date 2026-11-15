use std::time::Duration;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use url::Url;

use crate::db::Database;
use crate::models::{FeedSyncSummary, IngestionResult};

/// A downloaded story image, ready to draw.
#[derive(Clone)]
pub enum StoryImage {
    /// Decoded RGBA pixels. Decoding happens on a background thread: iced
    /// would otherwise decode on the UI thread during layout, for every image
    /// in the story at once, freezing the window on image-heavy stories.
    Raster { width: u32, height: u32, pixels: Vec<u8> },
    Svg(Vec<u8>),
}

// Hand-written so a message holding megabytes of pixels isn't dumped in full
impl std::fmt::Debug for StoryImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoryImage::Raster { width, height, .. } => write!(f, "Raster({}x{})", width, height),
            StoryImage::Svg(bytes) => write!(f, "Svg({} bytes)", bytes.len()),
        }
    }
}

/// Largest size a story image is kept at. The reading column is at most 760
/// logical pixels wide, so this stays sharp on 2x displays while capping
/// memory (a 6000x4000 photo would otherwise take ~96 MB as RGBA).
const MAX_IMAGE_WIDTH: u32 = 1600;
const MAX_IMAGE_HEIGHT: u32 = 4800;

/// Decodes (and if needed shrinks) an image. CPU-heavy: run it off the UI thread.
fn decode_image(bytes: &[u8]) -> Result<StoryImage, String> {
    let mut image = image::load_from_memory(bytes).map_err(|e| format!("can't decode: {}", e))?;
    if image.width() > MAX_IMAGE_WIDTH || image.height() > MAX_IMAGE_HEIGHT {
        image = image.resize(MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT, image::imageops::FilterType::Triangle);
    }
    let rgba = image.into_rgba8();
    Ok(StoryImage::Raster {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// How the app identifies itself to feed servers.
const USER_AGENT: &str = concat!("ShreaderRSS/", env!("CARGO_PKG_VERSION"), " (Linux)");

/// Largest feed document downloaded. Real feeds are well under 5 MB; this
/// only stops a wrong or hostile URL from filling memory.
const MAX_FEED_BYTES: usize = 25 * 1024 * 1024;

/// Largest story image downloaded.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// How many feeds are downloaded at once during a refresh (Akregator's
/// default "Concurrent Fetches").
const CONCURRENT_FETCHES: usize = 6;

/// Reads a response body, giving up once it passes `limit` bytes, whether
/// or not the server announced its size.
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if resp.content_length().is_some_and(|len| len > limit as u64) {
        return Err("too large".to_string());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("download interrupted: {}", e))?
    {
        if body.len() + chunk.len() > limit {
            return Err("too large".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parses a feed document.
///
/// An entry without an id gets one from feed-rs's usual link-and-title
/// scheme, so stories already stored keep matching. With no link either,
/// feed-rs would invent a random id on every download, making the same story
/// look new on each refresh; such entries are left without an id, and
/// `generate_guid_hash` identifies them by title and date instead.
fn parse_feed(bytes: &[u8]) -> feed_rs::parser::ParseFeedResult<feed_rs::model::Feed> {
    feed_rs::parser::Builder::new()
        .id_generator(|links, title, uri| {
            if links.is_empty() {
                String::new()
            } else {
                feed_rs::parser::generate_id(links, title, uri)
            }
        })
        .build()
        .parse(bytes)
}

/// What `FeedFetcher::preview` found at a URL.
#[derive(Debug, Clone)]
pub struct FeedPreview {
    pub title: Option<String>,
    pub story_count: usize,
}

pub struct FeedFetcher {
    client: reqwest::Client,
}

impl Default for FeedFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedFetcher {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { client }
    }

    /// Fetches and ingests a feed into the database, preserving all existing
    /// articles. Never fails as a whole: any problem is reported in the
    /// summary's `error`, so one bad feed can't stop a refresh of many.
    pub async fn fetch_and_sync(&self, db: &Database, feed_id: i64) -> FeedSyncSummary {
        match self.try_fetch_and_sync(db, feed_id).await {
            Ok(summary) => summary,
            Err(e) => FeedSyncSummary {
                feed_title: db
                    .get_feed(feed_id)
                    .ok()
                    .flatten()
                    .map(|f| f.title)
                    .unwrap_or_else(|| format!("Feed #{}", feed_id)),
                articles_inserted: 0,
                articles_updated: 0,
                articles_unchanged: 0,
                error: Some(format!("{:#}", e)),
            },
        }
    }

    /// Refreshes `feed_ids`, several at a time. One summary per feed, in no
    /// particular order.
    pub async fn sync_feeds(&self, db: &Database, feed_ids: Vec<i64>) -> Vec<FeedSyncSummary> {
        stream::iter(feed_ids)
            .map(|feed_id| self.fetch_and_sync(db, feed_id))
            .buffer_unordered(CONCURRENT_FETCHES)
            .collect()
            .await
    }

    async fn try_fetch_and_sync(&self, db: &Database, feed_id: i64) -> Result<FeedSyncSummary> {
        let feed = db
            .get_feed(feed_id)?
            .with_context(|| format!("Feed with ID {} not found", feed_id))?;

        let mut req = self.client.get(&feed.url);

        if let Some(ref etag) = feed.etag {
            req = req.header(IF_NONE_MATCH, etag);
        }
        if let Some(ref last_mod) = feed.last_modified {
            req = req.header(IF_MODIFIED_SINCE, last_mod);
        }

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                return Ok(FeedSyncSummary {
                    feed_title: feed.title.clone(),
                    articles_inserted: 0,
                    articles_updated: 0,
                    articles_unchanged: 0,
                    error: Some(format!("HTTP Request Failed: {}", e)),
                });
            }
        };

        let status = resp.status();

        // 304 Not Modified: Feed hasn't changed, no need to parse XML
        if status == StatusCode::NOT_MODIFIED {
            db.update_feed_http_meta(feed_id, feed.etag.as_deref(), feed.last_modified.as_deref(), Utc::now())?;
            return Ok(FeedSyncSummary {
                feed_title: feed.title,
                articles_inserted: 0,
                articles_updated: 0,
                articles_unchanged: 0,
                error: None,
            });
        }

        if !status.is_success() {
            return Ok(FeedSyncSummary {
                feed_title: feed.title.clone(),
                articles_inserted: 0,
                articles_updated: 0,
                articles_unchanged: 0,
                error: Some(format!("HTTP Error {}", status)),
            });
        }

        // Extract caching headers for future requests
        let new_etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let new_last_mod = resp.headers().get(LAST_MODIFIED).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

        let bytes = match read_limited(resp, MAX_FEED_BYTES).await {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(FeedSyncSummary {
                    feed_title: feed.title.clone(),
                    articles_inserted: 0,
                    articles_updated: 0,
                    articles_unchanged: 0,
                    error: Some(format!("Download failed: {}", e)),
                });
            }
        };

        // Parse feed using feed-rs
        let parsed_feed = match parse_feed(&bytes) {
            Ok(f) => f,
            Err(e) => {
                return Ok(FeedSyncSummary {
                    feed_title: feed.title.clone(),
                    articles_inserted: 0,
                    articles_updated: 0,
                    articles_unchanged: 0,
                    error: Some(format!("Feed Parse Error: {}", e)),
                });
            }
        };

        // Update feed metadata if available. Only the fields a refresh owns
        // are written, against the feed's current row: the user may have
        // renamed, moved or deleted it while it was downloading.
        let site_url = parsed_feed.links.first().map(|l| l.href.clone());
        let parsed_title = parsed_feed
            .title
            .as_ref()
            .map(|t| t.content.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|t| !t.is_empty());
        let description = parsed_feed.description.as_ref().map(|d| d.content.clone());
        let Some(feed_title) = db.update_feed_from_fetch(
            feed_id,
            parsed_title.as_deref(),
            description.as_deref(),
            site_url.as_deref(),
        )?
        else {
            // Deleted meanwhile: don't store its stories
            return Ok(FeedSyncSummary {
                feed_title: feed.title,
                articles_inserted: 0,
                articles_updated: 0,
                articles_unchanged: 0,
                error: None,
            });
        };
        db.update_feed_http_meta(feed_id, new_etag.as_deref(), new_last_mod.as_deref(), Utc::now())?;

        let mut inserted = 0;
        let mut updated = 0;
        let mut unchanged = 0;

        for entry in parsed_feed.entries {
            let title = entry.title.map(|t| t.content).unwrap_or_else(|| "Untitled".to_string());
            let link = entry.links.first().map(|l| clean_tracking_params(&l.href));
            let guid = if entry.id.trim().is_empty() {
                None
            } else {
                Some(entry.id)
            };

            // Generate canonical deduplication hash to guarantee no stories are lost or duplicated
            let guid_hash =
                generate_guid_hash(feed_id, guid.as_deref(), link.as_deref(), &title, entry.published);

            let author = if !entry.authors.is_empty() {
                Some(entry.authors.into_iter().map(|a| a.name).collect::<Vec<_>>().join(", "))
            } else {
                None
            };

            let summary = entry.summary.map(|s| s.content);
            let content_html = entry.content.and_then(|c| c.body);

            // Plain text conversion for reading and search
            let content_text = if let Some(ref html) = content_html {
                html2text::from_read(html.as_bytes(), 80).ok()
            } else if let Some(ref summ) = summary {
                html2text::from_read(summ.as_bytes(), 80).ok()
            } else {
                None
            };

            let published_at = entry.published;
            let updated_at = entry.updated;

            match db.upsert_article(
                feed_id,
                guid.as_deref(),
                &guid_hash,
                &title,
                link.as_deref(),
                author.as_deref(),
                summary.as_deref(),
                content_html.as_deref(),
                content_text.as_deref(),
                published_at,
                updated_at,
            ) {
                Ok(IngestionResult::Inserted) => inserted += 1,
                Ok(IngestionResult::Updated) => updated += 1,
                Ok(IngestionResult::Unchanged) => unchanged += 1,
                Err(e) => {
                    tracing::error!("Failed to upsert article '{}': {:?}", title, e);
                }
            }
        }

        Ok(FeedSyncSummary {
            feed_title,
            articles_inserted: inserted,
            articles_updated: updated,
            articles_unchanged: unchanged,
            error: None,
        })
    }

    /// Downloads and parses `url` without storing anything, so the Add Feed
    /// dialog can show the feed's title before subscribing.
    pub async fn preview(&self, url: &str) -> Result<FeedPreview, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "The site took too long to respond.".to_string()
                } else {
                    "Couldn't reach that address.".to_string()
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("The site answered with HTTP {}.", status.as_u16()));
        }

        let bytes = read_limited(resp, MAX_FEED_BYTES).await.map_err(|e| {
            if e == "too large" {
                "That address serves a file far too large to be a feed.".to_string()
            } else {
                "The download was interrupted.".to_string()
            }
        })?;
        let parsed = parse_feed(&bytes)
            .map_err(|_| "That address isn't an RSS or Atom feed.".to_string())?;

        let title = parsed
            .title
            .map(|t| t.content.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|t| !t.is_empty());

        Ok(FeedPreview {
            title,
            story_count: parsed.entries.len(),
        })
    }

    /// Downloads an image from a story. Refuses web pages and anything over
    /// `MAX_IMAGE_BYTES`.
    pub async fn fetch_image(&self, url: &str) -> Result<StoryImage, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/html") {
            return Err("not an image".to_string());
        }
        let is_svg = content_type.starts_with("image/svg")
            || Url::parse(url).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".svg"));

        let bytes = read_limited(resp, MAX_IMAGE_BYTES).await?;
        if is_svg {
            return Ok(StoryImage::Svg(bytes));
        }
        tokio::task::spawn_blocking(move || decode_image(&bytes))
            .await
            .map_err(|e| format!("decoding failed: {}", e))?
    }

    /// Refreshes every feed in the database (see `sync_feeds`).
    pub async fn sync_all_feeds(&self, db: &Database) -> Result<Vec<FeedSyncSummary>> {
        let feed_ids = db.get_feeds()?.into_iter().map(|f| f.id).collect();
        Ok(self.sync_feeds(db, feed_ids).await)
    }
}

/// Strips tracking query parameters (such as `utm_*`, `ref`, `fbclid`, `gclid`)
/// so links are normalized and stories are not duplicated when tracking codes change.
pub fn clean_tracking_params(raw_url: &str) -> String {
    if let Ok(mut parsed) = Url::parse(raw_url) {
        let filtered_pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| {
                let lower = k.to_lowercase();
                !lower.starts_with("utm_")
                    && lower != "ref"
                    && lower != "fbclid"
                    && lower != "gclid"
                    && lower != "mc_eid"
                    && lower != "mc_cid"
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        if filtered_pairs.is_empty() {
            parsed.set_query(None);
        } else {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in filtered_pairs {
                serializer.append_pair(&k, &v);
            }
            parsed.set_query(Some(&serializer.finish()));
        }
        parsed.to_string()
    } else {
        raw_url.to_string()
    }
}

/// Derives a deterministic canonical hash for an article.
/// Fallback order ensures an article has a stable identifier even if
/// the publisher doesn't supply a GUID, or changes URLs, or vice versa.
/// With neither, the title and publication date identify it (the date
/// keeps apart recurring titles like "Weekly update").
pub fn generate_guid_hash(
    feed_id: i64,
    guid: Option<&str>,
    link: Option<&str>,
    title: &str,
    published: Option<DateTime<Utc>>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(feed_id.to_le_bytes());

    if let Some(g) = guid.filter(|s| !s.trim().is_empty()) {
        hasher.update(b"guid:");
        hasher.update(g.trim().as_bytes());
    } else if let Some(l) = link.filter(|s| !s.trim().is_empty()) {
        hasher.update(b"link:");
        hasher.update(clean_tracking_params(l).as_bytes());
    } else {
        hasher.update(b"title:");
        hasher.update(title.trim().as_bytes());
        if let Some(published) = published {
            hasher.update(b"\npublished:");
            hasher.update(published.to_rfc3339().as_bytes());
        }
    }

    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_tracking_params() {
        let dirty = "https://example.com/article?utm_source=twitter&utm_medium=social&id=42&ref=newsletter";
        let clean = clean_tracking_params(dirty);
        assert_eq!(clean, "https://example.com/article?id=42");

        let pure_utm = "https://example.com/article?utm_source=rss";
        assert_eq!(clean_tracking_params(pure_utm), "https://example.com/article");
    }

    #[test]
    fn test_guid_hash_stability() {
        let hash1 = generate_guid_hash(1, Some("guid-123"), Some("https://example.com"), "Title", None);
        let hash2 = generate_guid_hash(1, Some("guid-123"), Some("https://different-link.com"), "Title 2", None);
        assert_eq!(hash1, hash2, "Same GUID should produce identical hash regardless of link");

        let hash_link1 = generate_guid_hash(1, None, Some("https://example.com/post?utm_source=rss"), "Title", None);
        let hash_link2 = generate_guid_hash(1, None, Some("https://example.com/post"), "Title", None);
        assert_eq!(hash_link1, hash_link2, "Same clean URL should produce identical hash");
    }

    /// Entries with no id and no link keep the same identity across
    /// downloads (feed-rs alone gives them a new random id each time), and
    /// entries with a link keep the id feed-rs has always given them.
    #[test]
    fn entries_without_id_or_link_keep_their_identity() {
        let feed = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title>
            <item><title>No link</title><description>a</description><pubDate>Mon, 02 Nov 2026 10:00:00 GMT</pubDate></item>
            <item><title>No link</title><description>b</description><pubDate>Mon, 09 Nov 2026 10:00:00 GMT</pubDate></item>
            <item><title>Linked</title><link>https://example.com/a</link></item>
            </channel></rss>"#;
        let ids = |feed: &feed_rs::model::Feed| -> Vec<String> {
            feed.entries
                .iter()
                .map(|e| {
                    let guid = Some(e.id.as_str()).filter(|id| !id.is_empty());
                    let link = e.links.first().map(|l| l.href.as_str());
                    let title = e.title.as_ref().map(|t| t.content.as_str()).unwrap_or("");
                    generate_guid_hash(1, guid, link, title, e.published)
                })
                .collect()
        };
        let first = parse_feed(feed).unwrap();
        let second = parse_feed(feed).unwrap();
        assert_eq!(ids(&first), ids(&second));
        // Same title, different dates: two stories
        assert_ne!(ids(&first)[0], ids(&first)[1]);
        // Linked entries: unchanged from feed-rs's default id
        assert_eq!(first.entries[2].id, feed_rs::parser::parse(&feed[..]).unwrap().entries[2].id);
        // And the default really is random for unlinked entries
        assert_ne!(
            feed_rs::parser::parse(&feed[..]).unwrap().entries[0].id,
            feed_rs::parser::parse(&feed[..]).unwrap().entries[0].id
        );
    }

    /// A tiny HTTP server for tests. Paths:
    /// /good (a feed), /missing (404), /drop (connection cut mid-body),
    /// /huge (an endless body with no announced size). Returns its base URL.
    async fn test_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                    let feed = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Good Feed</title><link>https://good.example</link>
                        <item><guid>1</guid><title>First</title><description>One</description></item>
                        <item><guid>2</guid><title>Second</title><description>Two</description></item>
                        </channel></rss>"#;
                    match path.as_str() {
                        "/good" => {
                            let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", feed.len(), feed).as_bytes()).await;
                        }
                        "/drop" => {
                            // Promises 100000 bytes, sends a few, hangs up
                            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n<?xml version=").await;
                        }
                        "/huge" => {
                            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await;
                            let chunk = vec![b'x'; 64 * 1024];
                            loop {
                                let header = format!("{:x}\r\n", chunk.len());
                                if sock.write_all(header.as_bytes()).await.is_err()
                                    || sock.write_all(&chunk).await.is_err()
                                    || sock.write_all(b"\r\n").await.is_err()
                                {
                                    break;
                                }
                            }
                        }
                        _ => {
                            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        }
                    }
                });
            }
        });
        base
    }

    /// One feed failing in any way doesn't stop the others from refreshing.
    #[tokio::test]
    async fn failing_feeds_dont_stop_a_refresh() {
        let base = test_server().await;
        let db = Database::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for path in ["drop", "missing", "huge", "good"] {
            let url = format!("{}/{}", base, path);
            ids.push(db.add_or_update_feed(&url, &url, None, None, None).unwrap());
        }
        let fetcher = FeedFetcher::new();
        let summaries = fetcher.sync_feeds(&db, ids.clone()).await;
        assert_eq!(summaries.len(), 4);

        let good = summaries.iter().find(|s| s.error.is_none()).expect("good feed refreshed");
        assert_eq!(good.feed_title, "Good Feed"); // placeholder title replaced
        assert_eq!(good.articles_inserted, 2);
        let errors: Vec<&str> = summaries.iter().filter_map(|s| s.error.as_deref()).collect();
        assert_eq!(errors.len(), 3, "{:?}", errors);
        assert!(errors.iter().any(|e| e.contains("too large")), "{:?}", errors);
        assert!(errors.iter().any(|e| e.contains("404")), "{:?}", errors);

        // A second refresh finds nothing new or changed
        let again = fetcher.sync_feeds(&db, vec![ids[3]]).await;
        assert_eq!((again[0].articles_inserted, again[0].articles_updated, again[0].articles_unchanged), (0, 0, 2));
    }
}
