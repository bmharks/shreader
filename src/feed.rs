use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
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

/// Fonts for SVG text: a cleaned SVG has its text turned into shapes, which
/// needs the fonts. Loaded once, on first use (off the UI thread).
static SVG_FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    Arc::new(fonts)
});

/// Makes a story's SVG safe to display.
///
/// iced draws SVGs with the SVG library's default settings, which load any
/// local file an `<image>` element names, on the UI thread. A feed could use
/// that to freeze the app (a named pipe), exhaust memory (`/dev/zero`) or
/// show files from the disk. So the SVG is parsed here first with file
/// loading off (images embedded as data still work), and iced gets the
/// re-written result, which contains no file references at all.
fn clean_svg(bytes: &[u8]) -> Result<StoryImage, String> {
    let mut options = usvg::Options {
        fontdb: SVG_FONTS.clone(),
        ..usvg::Options::default()
    };
    options.image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(|_, _| None),
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|e| format!("unusable SVG: {}", e))?;
    Ok(StoryImage::Svg(tree.to_string(&usvg::WriteOptions::default()).into_bytes()))
}

/// Whether `ip` is on the public internet, as opposed to this machine, the
/// local network or other special ranges.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()            // 127/8
                || v4.is_private()        // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()     // 169.254/16 (incl. cloud metadata)
                || v4.is_unspecified()    // 0.0.0.0
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0                 // 0/8 "this network"
                || (a == 100 && (64..128).contains(&b)) // 100.64/10 shared (CGNAT, Tailscale)
                || (a == 198 && (18..20).contains(&b))  // 198.18/15 benchmarking
                || a >= 240)              // reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00  // fc00::/7 unique local
                || (first & 0xffc0) == 0xfe80  // fe80::/10 link-local
                || (first & 0xffc0) == 0xfec0) // fec0::/10 site-local (deprecated)
        }
    }
}

/// Whether a story image may be downloaded from `url`: web addresses only,
/// and not a numeric address on this machine or the local network. (Names
/// are checked when they're looked up; see `PublicOnlyResolver`.)
fn image_url_allowed(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(url::Host::Ipv4(ip)) => is_public_ip(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => is_public_ip(IpAddr::V6(ip)),
            Some(url::Host::Domain(_)) => true,
            None => false,
        }
}

/// Looks up names for story image downloads, keeping only public addresses.
/// Checking at lookup time (rather than before the request) means a name
/// can't pass a check and then point somewhere local when connecting.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public_ip(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{} isn't a public internet address", host).into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
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
    /// For images inside stories: only reaches public internet addresses.
    /// A story's `<img>` pointing at a router or a local service would
    /// otherwise make the app send it a request when scrolled into view.
    /// (Feeds use `client`, so subscribing to a feed on the local network
    /// still works.)
    image_client: reqwest::Client,
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

        let image_client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .dns_resolver(Arc::new(PublicOnlyResolver))
            // Redirects to numeric addresses skip the name lookup; check them
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if !image_url_allowed(attempt.url()) {
                    attempt.error("redirected to a local or non-web address")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .expect("the image download client's settings are valid");

        Self { client, image_client }
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
        let parsed = Url::parse(url).map_err(|e| format!("bad URL: {}", e))?;
        if !image_url_allowed(&parsed) {
            return Err("not a public web address".to_string());
        }
        let resp = self
            .image_client
            .get(parsed)
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
        // The server's stated type wins: image CDNs often serve a PNG
        // version of a link ending in .svg. The extension only decides when
        // the type says nothing useful.
        let is_svg = content_type.starts_with("image/svg")
            || (!content_type.starts_with("image/")
                && Url::parse(url).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".svg")));

        let bytes = read_limited(resp, MAX_IMAGE_BYTES).await?;
        tokio::task::spawn_blocking(move || if is_svg { clean_svg(&bytes) } else { decode_image(&bytes) })
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

    fn svg_text(image: &StoryImage) -> String {
        match image {
            StoryImage::Svg(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            other => panic!("expected an SVG, got {:?}", other),
        }
    }

    /// A story SVG can't pull in local files; embedded images still work.
    #[test]
    fn story_svgs_cant_read_local_files() {
        let dir = std::env::temp_dir().join(format!("shreader-svg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png_path = dir.join("local.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255])).save(&png_path).unwrap();
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageOutputFormat::Png)
            .unwrap();
        use base64_free::encode;
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
                <image href="{}" width="10" height="10"/>
                <image x="10" href="data:image/png;base64,{}" width="10" height="10"/>
            </svg>"#,
            png_path.display(),
            encode(&png_bytes)
        );

        // What iced would do with it unchecked: the local file is loaded
        let unchecked = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).unwrap();
        let unchecked = unchecked.to_string(&usvg::WriteOptions::default());
        assert_eq!(unchecked.matches("data:image/png").count(), 2);

        // Cleaned: only the embedded image is left
        let cleaned = svg_text(&clean_svg(svg.as_bytes()).unwrap());
        assert_eq!(cleaned.matches("data:image/png").count(), 1, "{}", cleaned);
        assert!(!cleaned.contains("local.png"));
        // And the result is itself a valid SVG
        usvg::Tree::from_data(cleaned.as_bytes(), &usvg::Options::default()).unwrap();

        // A named pipe would block the reader forever; cleaning doesn't touch it
        let pipe = dir.join("pipe");
        let c_path = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        // SAFETY: plain libc call with a valid NUL-terminated path
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/><image href="{}" width="10" height="10"/></svg>"#,
            pipe.display()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(clean_svg(svg.as_bytes()).is_ok());
        });
        let finished = rx.recv_timeout(Duration::from_secs(5));
        if finished.is_err() {
            // Unblock the stuck thread before failing
            let _ = std::fs::OpenOptions::new().write(true).open(&pipe);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(finished, Ok(true), "cleaning an SVG that names a pipe must not hang");
    }

    /// Tiny base64 encoder for the test above (no extra dependency).
    mod base64_free {
        pub fn encode(bytes: &[u8]) -> String {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }
    }

    #[test]
    fn public_and_local_addresses() {
        let public = ["93.184.215.14", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "100.128.0.1"];
        let local = [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254",
            "0.0.0.0", "100.64.0.1", "100.127.255.255", "255.255.255.255", "224.0.0.1",
            "::1", "::", "fe80::1", "fd12:3456::1", "::ffff:192.168.1.1", "::ffff:127.0.0.1",
        ];
        for ip in public {
            assert!(is_public_ip(ip.parse().unwrap()), "{} should be public", ip);
        }
        for ip in local {
            assert!(!is_public_ip(ip.parse().unwrap()), "{} should be local", ip);
        }
        let allowed = |u: &str| image_url_allowed(&Url::parse(u).unwrap());
        assert!(allowed("https://example.com/a.png"));
        assert!(allowed("http://8.8.8.8/a.png"));
        assert!(!allowed("http://192.168.1.1/reboot"));
        assert!(!allowed("http://[::1]:8080/a.png"));
        assert!(!allowed("file:///etc/passwd"));
    }

    /// Story images aren't downloaded from this machine, whether the address
    /// is numeric or a name that resolves to it; feeds still are.
    #[tokio::test]
    async fn story_images_stay_off_the_local_network() {
        let base = test_server().await; // http://127.0.0.1:PORT
        let fetcher = FeedFetcher::new();
        let numeric = fetcher.fetch_image(&format!("{}/good", base)).await;
        assert!(numeric.is_err());
        let named = fetcher.fetch_image(&base.replace("127.0.0.1", "localhost")).await;
        let error = named.unwrap_err();
        assert!(error.contains("request failed"), "{}", error);
        // The feed client is unaffected
        assert!(fetcher.preview(&format!("{}/good", base)).await.is_ok());
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
