use std::time::Duration;
use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, USER_AGENT};
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
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { client }
    }

    /// Fetches and ingests a feed into the database, preserving all existing articles.
    pub async fn fetch_and_sync(&self, db: &Database, feed_id: i64) -> Result<FeedSyncSummary> {
        let feed = db
            .get_feed(feed_id)?
            .with_context(|| format!("Feed with ID {} not found", feed_id))?;

        let mut req = self
            .client
            .get(&feed.url)
            .header(USER_AGENT, "RSSReader/0.1 (Linux; Rust; +https://github.com/system76/rss_reader)");

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

        let bytes = resp.bytes().await.context("Failed to read response body")?;

        // Parse feed using feed-rs
        let parsed_feed = match feed_rs::parser::parse(&bytes[..]) {
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

        // Update feed metadata if available
        let site_url = parsed_feed.links.first().map(|l| l.href.clone());
        let feed_title = if feed.title.is_empty() || feed.title == feed.url {
            parsed_feed.title.as_ref().map(|t| t.content.clone()).unwrap_or_else(|| feed.title.clone())
        } else {
            feed.title.clone()
        };

        let description = parsed_feed.description.as_ref().map(|d| d.content.clone());
        db.add_or_update_feed(&feed.url, &feed_title, description.as_deref(), site_url.as_deref(), feed.folder_id)?;
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
            let guid_hash = generate_guid_hash(feed_id, guid.as_deref(), link.as_deref(), &title);

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
            .header(USER_AGENT, "RSSReader/0.1 (Linux; Rust; +https://github.com/system76/rss_reader)")
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

        let bytes = resp
            .bytes()
            .await
            .map_err(|_| "The download was interrupted.".to_string())?;
        let parsed = feed_rs::parser::parse(&bytes[..])
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
        const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

        let resp = self
            .client
            .get(url)
            .header(USER_AGENT, "RSSReader/0.1 (Linux; Rust; +https://github.com/system76/rss_reader)")
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|len| len > MAX_IMAGE_BYTES) {
            return Err("image too large".to_string());
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

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("download interrupted: {}", e))?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err("image too large".to_string());
        }
        if is_svg {
            return Ok(StoryImage::Svg(bytes.to_vec()));
        }
        tokio::task::spawn_blocking(move || decode_image(&bytes))
            .await
            .map_err(|e| format!("decoding failed: {}", e))?
    }

    /// Fetches all feeds currently in the database concurrently
    pub async fn sync_all_feeds(&self, db: &Database) -> Result<Vec<FeedSyncSummary>> {
        let feeds = db.get_feeds()?;
        let mut summaries = Vec::new();

        for feed in feeds {
            let summary = self.fetch_and_sync(db, feed.id).await?;
            summaries.push(summary);
        }

        Ok(summaries)
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
pub fn generate_guid_hash(
    feed_id: i64,
    guid: Option<&str>,
    link: Option<&str>,
    title: &str,
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
        let hash1 = generate_guid_hash(1, Some("guid-123"), Some("https://example.com"), "Title");
        let hash2 = generate_guid_hash(1, Some("guid-123"), Some("https://different-link.com"), "Title 2");
        assert_eq!(hash1, hash2, "Same GUID should produce identical hash regardless of link");

        let hash_link1 = generate_guid_hash(1, None, Some("https://example.com/post?utm_source=rss"), "Title");
        let hash_link2 = generate_guid_hash(1, None, Some("https://example.com/post"), "Title");
        assert_eq!(hash_link1, hash_link2, "Same clean URL should produce identical hash");
    }
}
