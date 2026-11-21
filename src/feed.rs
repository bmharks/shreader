use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use resvg::usvg;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use url::Url;

use crate::db::Database;
use crate::db::StoredState;
use crate::models::{FeedSyncSummary, IngestionResult};

/// A downloaded story image, ready to draw.
#[derive(Clone)]
pub enum StoryImage {
    /// Decoded RGBA pixels. Decoding happens on a background thread: iced
    /// would otherwise decode on the UI thread during layout, for every image
    /// in the story at once, freezing the window on image-heavy stories.
    /// (SVGs are drawn into pixels too: see `render_svg`.)
    Raster { width: u32, height: u32, pixels: Vec<u8> },
}

// Hand-written so a message holding megabytes of pixels isn't dumped in full
impl std::fmt::Debug for StoryImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoryImage::Raster { width, height, .. } => write!(f, "Raster({}x{})", width, height),
        }
    }
}

/// Largest size a story image is kept at. The reading column is at most 760
/// logical pixels wide, so this stays sharp on 2x displays while capping
/// memory (a 6000x4000 photo would otherwise take ~96 MB as RGBA).
const MAX_IMAGE_WIDTH: u32 = 1600;
const MAX_IMAGE_HEIGHT: u32 = 4800;

/// Largest image decoded, in pixels: a 50-megapixel camera photo, about
/// 200 MB while decoding. A small file can declare a far larger image
/// ("decompression bomb"); those are refused before decoding.
const MAX_IMAGE_PIXELS: u64 = 50_000_000;

/// Memory a single decode may use (a backstop; not every format honours it).
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// Images decoded at once, so a story full of large images can't stack up
/// several gigabytes of decoding at the same moment.
static DECODE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Decodes (and if needed shrinks) an image. CPU-heavy: run it off the UI thread.
fn decode_image(bytes: &[u8]) -> Result<StoryImage, String> {
    let reader = || {
        image::io::Reader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| format!("can't read: {}", e))
    };
    // The header alone gives the size: check it before allocating anything
    let (width, height) = reader()?.into_dimensions().map_err(|e| format!("can't decode: {}", e))?;
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(format!("too large ({}x{})", width, height));
    }
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let mut decoder = reader()?;
    decoder.limits(limits);
    let mut image = decoder.decode().map_err(|e| format!("can't decode: {}", e))?;
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

/// Fonts for text in story SVGs. Loaded once, on first use (off the UI thread).
static SVG_FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    Arc::new(fonts)
});

/// Draws a story's SVG into an ordinary image, off the UI thread.
///
/// Story SVGs never reach iced as SVGs. iced draws them with the SVG
/// library's default settings, which load any local file an `<image>`
/// element names, on the UI thread: a feed could freeze the app (a named
/// pipe), exhaust memory (`/dev/zero`) or show files from the disk. Parsing
/// and drawing can also simply take long (thousands of text labels: seconds;
/// `<use>` chains that expand a 1 KB file into 100,000 shapes). Here file
/// loading is off, embedded images get the same size check as photos, and the
/// work happens in the background; the reader gets pixels.
fn render_svg(bytes: &[u8]) -> Result<StoryImage, String> {
    let mut options = usvg::Options {
        fontdb: SVG_FONTS.clone(),
        ..usvg::Options::default()
    };
    let embedded = usvg::ImageHrefResolver::default_data_resolver();
    options.image_href_resolver = usvg::ImageHrefResolver {
        // Images embedded as data: refuse decompression bombs
        resolve_data: Box::new(move |mime, data, opts| {
            let too_big = image::io::Reader::new(std::io::Cursor::new(data.as_slice()))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.into_dimensions().ok())
                .is_some_and(|(w, h)| u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS);
            if too_big { None } else { embedded(mime, data, opts) }
        }),
        // Anything else is a file path (or an unsupported URL): never loaded
        resolve_string: Box::new(|_, _| None),
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|e| format!("unusable SVG: {}", e))?;

    // Its own size, shrunk to the same limits as photos
    let size = tree.size();
    let scale = (MAX_IMAGE_WIDTH as f32 / size.width())
        .min(MAX_IMAGE_HEIGHT as f32 / size.height())
        .min(1.0);
    let width = ((size.width() * scale).ceil() as u32).max(1);
    let height = ((size.height() * scale).ceil() as u32).max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or("empty SVG")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());

    // tiny-skia stores premultiplied alpha; iced expects straight RGBA
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Ok(StoryImage::Raster { width, height, pixels })
}

/// Whether `ip` is on the public internet, as opposed to this machine, the
/// local network or other special ranges: as IANA's special-purpose address
/// registries say (RFC 6890; iana.org/assignments/iana-ipv4-special-registry
/// and iana-ipv6-special-registry, as of 2026), plus multicast and the
/// deprecated ranges a public server would never use. An IPv4 address
/// carried inside an IPv6 one is judged as itself.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let bits = v4.to_bits();
            let within = |&(net, len): &(Ipv4Addr, u32)| len == 0 || bits >> (32 - len) == net.to_bits() >> (32 - len);
            V4_GLOBAL_EXCEPTIONS.iter().any(within) || !V4_NOT_GLOBAL.iter().any(within)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let bits = v6.to_bits();
            // NAT64 (64:ff9b::/96): an IPv4 address in the last 32 bits, which
            // the network's gateway translates to, local ones included
            if bits >> 32 == Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0).to_bits() >> 32 {
                return is_public_ip(IpAddr::V4(Ipv4Addr::from_bits(bits as u32)));
            }
            let within = |&(net, len): &(Ipv6Addr, u32)| len == 0 || bits >> (128 - len) == net.to_bits() >> (128 - len);
            V6_GLOBAL_EXCEPTIONS.iter().any(within) || !V6_NOT_GLOBAL.iter().any(within)
        }
    }
}

/// IPv4 ranges that aren't reachable on the public internet (IANA:
/// "Globally Reachable: False"), plus multicast and the deprecated 6to4 relay
const V4_NOT_GLOBAL: &[(Ipv4Addr, u32)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),         // "this network" (RFC 791)
    (Ipv4Addr::new(10, 0, 0, 0), 8),        // private (RFC 1918)
    (Ipv4Addr::new(100, 64, 0, 0), 10),     // shared: carrier NAT, Tailscale (RFC 6598)
    (Ipv4Addr::new(127, 0, 0, 0), 8),       // loopback (RFC 1122)
    (Ipv4Addr::new(169, 254, 0, 0), 16),    // link-local, cloud metadata (RFC 3927)
    (Ipv4Addr::new(172, 16, 0, 0), 12),     // private (RFC 1918)
    (Ipv4Addr::new(192, 0, 0, 0), 24),      // IETF protocol assignments (RFC 6890)
    (Ipv4Addr::new(192, 0, 2, 0), 24),      // documentation (RFC 5737)
    (Ipv4Addr::new(192, 88, 99, 0), 24),    // 6to4 relay anycast, deprecated (RFC 7526)
    (Ipv4Addr::new(192, 168, 0, 0), 16),    // private (RFC 1918)
    (Ipv4Addr::new(198, 18, 0, 0), 15),     // benchmarking (RFC 2544)
    (Ipv4Addr::new(198, 51, 100, 0), 24),   // documentation (RFC 5737)
    (Ipv4Addr::new(203, 0, 113, 0), 24),    // documentation (RFC 5737)
    (Ipv4Addr::new(224, 0, 0, 0), 4),       // multicast (RFC 5771)
    (Ipv4Addr::new(240, 0, 0, 0), 4),       // reserved, and broadcast (RFC 1112, RFC 919)
];

/// Single addresses inside those ranges that are public services
const V4_GLOBAL_EXCEPTIONS: &[(Ipv4Addr, u32)] = &[
    (Ipv4Addr::new(192, 0, 0, 9), 32),  // Port Control Protocol anycast (RFC 7723)
    (Ipv4Addr::new(192, 0, 0, 10), 32), // TURN anycast (RFC 8155)
];

/// IPv6 ranges that aren't reachable on the public internet (IANA:
/// "Globally Reachable: False"), plus multicast, the deprecated ranges, and
/// the tunnels that carry an IPv4 address (6to4, Teredo)
const V6_NOT_GLOBAL: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),                   // unspecified, loopback, IPv4-compatible (deprecated, RFC 4291)
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),           // local-use IPv4/IPv6 translation (RFC 8215)
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),               // discard-only (RFC 6666)
    (Ipv6Addr::new(0x100, 0, 0, 1, 0, 0, 0, 0), 64),               // dummy prefix (RFC 9780)
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),              // IETF protocol assignments, Teredo (RFC 2928, RFC 4380)
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),          // documentation (RFC 3849)
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),              // 6to4, deprecated (RFC 3056)
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),              // documentation (RFC 9637)
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),              // segment routing (SRv6) SIDs (RFC 9602)
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),               // unique local (RFC 4193)
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),              // link-local (RFC 4291)
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),              // site-local, deprecated (RFC 3879)
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),               // multicast (RFC 4291)
];

/// Addresses inside those ranges that are public services
const V6_GLOBAL_EXCEPTIONS: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1), 128),   // Port Control Protocol anycast (RFC 7723)
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2), 128),   // TURN anycast (RFC 8155)
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 3), 128),   // DNS-SD service registration anycast (RFC 9665)
    (Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0), 32),    // AMT (RFC 7450)
    (Ipv6Addr::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0), 48), // AS112-v6 (RFC 7535)
    (Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28), // ORCHIDv2 (RFC 7343)
    (Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0), 28), // drone remote ID (RFC 9374)
];

/// Whether a public download (a story image, a public feed, a redirect) may
/// go to `url`: web addresses only, and not a numeric address on this
/// machine or the local network. (Names
/// are checked when they're looked up; see `PublicOnlyResolver`.)
fn public_url_allowed(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(url::Host::Ipv4(ip)) => is_public_ip(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => is_public_ip(IpAddr::V6(ip)),
            Some(url::Host::Domain(_)) => true,
            None => false,
        }
}

/// Whether `url` names this machine or the local network directly: a
/// non-public IP address, `localhost`, a one-word name like `nas`, or a name
/// under a suffix reserved for local networks.
fn is_local_address(url: &Url) -> bool {
    const LOCAL_SUFFIXES: &[&str] = &[
        ".localhost", ".local", ".lan", ".home", ".home.arpa", ".internal", ".localdomain",
        ".intranet", ".corp", ".private",
    ];
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !is_public_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || !name.contains('.') || LOCAL_SUFFIXES.iter().any(|s| name.ends_with(s))
        }
        None => false,
    }
}

/// Whether downloads go through a proxy (reqwest reads these variables).
fn proxy_configured() -> bool {
    ["http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
}

/// A client that only reaches public internet addresses: names are checked
/// when looked up (`PublicOnlyResolver`, unless `check_names` is off), and
/// redirects to numeric local addresses are refused.
fn public_client(check_names: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        // Redirects to numeric addresses skip the name lookup; check them
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if !public_url_allowed(attempt.url()) {
                attempt.error("redirected to a local or non-web address")
            } else {
                attempt.follow()
            }
        }));
    if check_names {
        builder = builder.dns_resolver(Arc::new(PublicOnlyResolver));
    }
    builder.build().expect("the download client's settings are valid")
}

/// Looks up names for downloads, keeping only public addresses.
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
const USER_AGENT: &str = concat!("Shreader/", env!("CARGO_PKG_VERSION"), " (+https://github.com/bmharks/shreader)");

/// Largest feed document downloaded. Real feeds are well under 5 MB; this
/// only stops a wrong or hostile URL from filling memory.
const MAX_FEED_BYTES: usize = 25 * 1024 * 1024;

/// Largest story image downloaded.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Longest story title stored, in characters
pub(crate) const MAX_TITLE_CHARS: usize = 500;
/// Longest author line stored, in characters
pub(crate) const MAX_AUTHOR_CHARS: usize = 300;
/// Largest story body (and summary) stored. The largest real story seen was
/// 175 KB; anything bigger is cut, with a note.
const MAX_STORY_BYTES: usize = 512 * 1024;
/// Longest link kept
const MAX_URL_BYTES: usize = 8 * 1024;
/// New stories one feed may add in one refresh. A feed is a few MB at most,
/// but that can hold 200,000 tiny stories; with fresh ids each time, the
/// permanent archive would keep them all. The rest wait for later refreshes.
pub const MAX_NEW_PER_REFRESH: usize = 1000;
/// New stories, and bytes of them, one feed may add per day (UTC)
const MAX_NEW_PER_DAY: u64 = 3000;
const MAX_BYTES_PER_DAY: u64 = 100 * 1024 * 1024;

/// One line of text from a feed (a title, an author): control characters
/// (newlines, terminal escape codes) become spaces, invisible text-direction
/// overrides are removed (they can make a title display as something else),
/// and it's cut to `max_chars` characters.
pub(crate) fn clean_line(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !is_direction_control(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// How a line of text in a feed is written, which decides how it's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Markup {
    /// HTML: tags are dropped and character references decoded, as a
    /// browser would (Atom's `type="html"` and `type="xhtml"`)
    Html,
    /// Text with HTML character references (`&#8217;`, `&hellip;`) decoded,
    /// but a `<` always kept as text. RSS doesn't say whether its titles
    /// are HTML; in practice they're often double-escaped, so references
    /// need decoding, while "Why <iframe> is risky" is plain text.
    References,
    /// Plain text, shown as sent (Atom's `type="text"`, JSON Feed)
    Plain,
}

impl Markup {
    /// How the title `text` from a feed of `feed_type` is written.
    pub(crate) fn of_title(text: &feed_rs::model::Text, feed_type: &feed_rs::model::FeedType) -> Self {
        use feed_rs::model::FeedType;
        if text.content_type.to_string().starts_with("text/html") {
            Markup::Html
        } else if matches!(feed_type, FeedType::RSS0 | FeedType::RSS1 | FeedType::RSS2) {
            Markup::References
        } else {
            Markup::Plain
        }
    }
}

/// One line of feed text (a title, an author) as plain text to show:
/// read as `markup` says (The Verge's "Google&#8217;s" is "Google’s"), then
/// cleaned like any line (`clean_line`). No-break spaces become plain
/// spaces, so a title can still wrap.
pub(crate) fn feed_line(text: &str, markup: Markup, max_chars: usize) -> String {
    let text = match markup {
        Markup::Html => html_text(text),
        Markup::References => html_text(&text.replace('<', "&lt;")),
        Markup::Plain => text.to_string(),
    };
    clean_line(&text.replace('\u{a0}', " "), max_chars)
}

/// The text of an HTML fragment, as the HTML standard reads it: html5ever's
/// tokenizer (the one html2text uses for stories) decodes character
/// references, including those without their ";" and the old Windows
/// numbers ("&#146;" is ’), and the tags and comments are left out.
fn html_text(html: &str) -> String {
    use html5ever::tokenizer::{BufferQueue, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};

    struct Collect(std::cell::RefCell<String>);
    impl TokenSink for Collect {
        type Handle = ();
        fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
            if let Token::CharacterTokens(text) = token {
                self.0.borrow_mut().push_str(&text);
            }
            TokenSinkResult::Continue
        }
    }

    let input = BufferQueue::default();
    input.push_back(html5ever::tendril::StrTendril::from_slice(html));
    let tokenizer = Tokenizer::new(Collect(Default::default()), TokenizerOpts::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.0.into_inner()
}

/// Unicode's invisible bidirectional formatting characters: marks,
/// embeddings, overrides and isolates. Right-to-left text displays
/// correctly without them.
fn is_direction_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// A story body or summary, cut to `MAX_STORY_BYTES` with a note saying so.
fn cap_story(html: String) -> String {
    if html.len() <= MAX_STORY_BYTES {
        return html;
    }
    let mut cut = MAX_STORY_BYTES;
    while !html.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}<p><em>[Shortened: this story is larger than Shreader stores ({} KiB). Open it in the browser for the rest.]</em></p>",
        &html[..cut],
        MAX_STORY_BYTES / 1024
    )
}

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
    /// For feeds: only reaches public internet addresses, so a public feed
    /// can't redirect (or point its name) to a router or a service on this
    /// machine and have the app request it on every refresh.
    client: reqwest::Client,
    /// For feeds whose own address is local (`is_local_address`), which the
    /// user subscribed to on purpose
    local_client: reqwest::Client,
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
        // A configured proxy looks names up itself, and is often on this
        // machine, so the address check can't apply; redirects are still
        // checked.
        let check_addresses = !proxy_configured();
        if !check_addresses {
            tracing::info!("A proxy is set: public-address checks apply to redirects only");
        }
        Self {
            client: public_client(check_addresses),
            local_client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .expect("the local feed client's settings are valid"),
            image_client: public_client(check_addresses),
        }
    }

    /// The client for downloading the feed at `url`.
    fn feed_client(&self, url: &str) -> &reqwest::Client {
        if Url::parse(url).is_ok_and(|u| is_local_address(&u)) {
            &self.local_client
        } else {
            &self.client
        }
    }

    /// Fetches and ingests a feed into the database, preserving all existing
    /// articles. Never fails as a whole: any problem is reported in the
    /// summary's `error`, so one bad feed can't stop a refresh of many.
    ///
    /// That includes a panic (a bug, possibly in a library, triggered by odd
    /// feed content): otherwise the whole refresh would never report back,
    /// leaving the app stuck "syncing" with refreshes disabled.
    pub async fn fetch_and_sync(&self, db: &Database, feed_id: i64) -> FeedSyncSummary {
        let outcome = AssertUnwindSafe(self.try_fetch_and_sync(db, feed_id))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error while processing this feed")));
        match outcome {
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
                limited: false,
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

        let mut req = self.feed_client(&feed.url).get(&feed.url);

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
                    limited: false,
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
                limited: false,
            });
        }

        if !status.is_success() {
            return Ok(FeedSyncSummary {
                feed_title: feed.title.clone(),
                articles_inserted: 0,
                articles_updated: 0,
                articles_unchanged: 0,
                error: Some(format!("HTTP Error {}", status)),
                limited: false,
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
                    limited: false,
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
                    limited: false,
                });
            }
        };

        // Update feed metadata if available. Only the fields a refresh owns
        // are written, against the feed's current row: the user may have
        // renamed, moved or deleted it while it was downloading.
        let site_url = parsed_feed.links.first().map(|l| l.href.clone()).filter(|u| u.len() <= MAX_URL_BYTES);
        let parsed_title = parsed_feed
            .title
            .as_ref()
            .map(|t| {
                feed_line(&t.content, Markup::of_title(t, &parsed_feed.feed_type), MAX_TITLE_CHARS)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|t| !t.is_empty());
        let description = parsed_feed.description.as_ref().map(|d| clean_line(&d.content, 10_000));
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
                limited: false,
            });
        };
        db.update_feed_http_meta(feed_id, new_etag.as_deref(), new_last_mod.as_deref(), Utc::now())?;

        let mut inserted = 0;
        let mut updated = 0;
        let mut unchanged = 0;

        // What this feed may still add today, and in this refresh
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let (used_stories, used_bytes) = db.intake(feed_id, &today)?;
        let mut stories_left = MAX_NEW_PER_DAY.saturating_sub(used_stories);
        let mut bytes_left = MAX_BYTES_PER_DAY.saturating_sub(used_bytes);
        let mut new_bytes: u64 = 0;
        let mut limited = false;

        for entry in parsed_feed.entries {
            // The title as sent identifies a story without an id or link
            // (`generate_guid_hash`), so that stays as it was; it's shown decoded
            let title = entry
                .title
                .as_ref()
                .map(|t| feed_line(&t.content, Markup::of_title(t, &parsed_feed.feed_type), MAX_TITLE_CHARS));
            let raw_title = entry.title.map(|t| t.content).unwrap_or_else(|| "Untitled".to_string());
            let link = entry.links.first().map(|l| l.href.clone());
            let guid = if entry.id.trim().is_empty() {
                None
            } else {
                Some(entry.id)
            };

            // Generate canonical deduplication hash to guarantee no stories are lost or duplicated
            let guid_hash =
                generate_guid_hash(feed_id, guid.as_deref(), link.as_deref(), &raw_title, entry.published);

            // Stored within sane sizes: an enormous title or story makes the
            // list and reader unusable (a 5 MB story: 2 s per click)
            let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| clean_line(&raw_title, MAX_TITLE_CHARS));
            let link = link.filter(|l| l.len() <= MAX_URL_BYTES);
            let author = if !entry.authors.is_empty() {
                let names = entry.authors.into_iter().map(|a| a.name).collect::<Vec<_>>().join(", ");
                // Names have no declared type; WordPress sends "O&#8217;Brien"
                Some(feed_line(&names, Markup::References, MAX_AUTHOR_CHARS))
            } else {
                None
            };
            let summary = entry.summary.map(|s| cap_story(s.content));
            let content_html = entry.content.and_then(|c| c.body).map(cap_story);

            let state = db.stored_state(feed_id, &guid_hash, &title, summary.as_deref(), content_html.as_deref())?;
            let size = (title.len()
                + summary.as_ref().map_or(0, String::len)
                + content_html.as_ref().map_or(0, String::len)) as u64;
            if state == StoredState::New
                && (inserted >= MAX_NEW_PER_REFRESH || stories_left == 0 || size > bytes_left)
            {
                // Over the allowance: held back. A later refresh picks it up
                // if the feed still lists it.
                limited = true;
                continue;
            }

            // Plain text for reading and search; not needed (and skipped,
            // since converting is the slow part) when nothing changed
            let content_text = if state == StoredState::Unchanged {
                None
            } else {
                content_html.as_deref().or(summary.as_deref()).and_then(|html| crate::story_content::plain_text(html, 80))
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
                Ok(IngestionResult::Inserted) => {
                    inserted += 1;
                    stories_left = stories_left.saturating_sub(1);
                    bytes_left = bytes_left.saturating_sub(size);
                    new_bytes += size;
                }
                Ok(IngestionResult::Updated) => updated += 1,
                Ok(IngestionResult::Unchanged) => unchanged += 1,
                Err(e) => {
                    tracing::error!("Failed to upsert article '{}': {:?}", title, e);
                }
            }
        }
        if inserted > 0 {
            db.add_intake(feed_id, &today, inserted as u64, new_bytes)?;
        }

        Ok(FeedSyncSummary {
            feed_title,
            articles_inserted: inserted,
            articles_updated: updated,
            articles_unchanged: unchanged,
            error: None,
            limited,
        })
    }

    /// Downloads and parses `url` without storing anything, so the Add Feed
    /// dialog can show the feed's title before subscribing.
    pub async fn preview(&self, url: &str) -> Result<FeedPreview, String> {
        AssertUnwindSafe(self.try_preview(url))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("Something went wrong reading that feed.".to_string()))
    }

    async fn try_preview(&self, url: &str) -> Result<FeedPreview, String> {
        let resp = self
            .feed_client(url)
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
            .map(|t| {
                feed_line(&t.content, Markup::of_title(&t, &parsed.feed_type), MAX_TITLE_CHARS)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
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
        if !public_url_allowed(&parsed) {
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
        let _slot = DECODE_SLOTS.acquire().await.map_err(|e| e.to_string())?;
        tokio::task::spawn_blocking(move || if is_svg { render_svg(&bytes) } else { decode_image(&bytes) })
            .await
            .map_err(|e| format!("decoding failed: {}", e))?
    }

    /// Refreshes every feed in the database (see `sync_feeds`).
    pub async fn sync_all_feeds(&self, db: &Database) -> Result<Vec<FeedSyncSummary>> {
        let feed_ids = db.get_feeds()?.into_iter().map(|f| f.id).collect();
        Ok(self.sync_feeds(db, feed_ids).await)
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
        hasher.update(l.trim().as_bytes());
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
    fn test_guid_hash_stability() {
        let hash1 = generate_guid_hash(1, Some("guid-123"), Some("https://example.com"), "Title", None);
        let hash2 = generate_guid_hash(1, Some("guid-123"), Some("https://different-link.com"), "Title 2", None);
        assert_eq!(hash1, hash2, "Same GUID should produce identical hash regardless of link");

        let hash_link1 = generate_guid_hash(1, None, Some("https://example.com/post"), "Title", None);
        let hash_link2 = generate_guid_hash(1, None, Some("https://example.com/post"), "Title 2", None);
        assert_eq!(hash_link1, hash_link2, "Without a GUID, the same link is the same story");
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

    fn pixel(image: &StoryImage, x: u32, y: u32) -> [u8; 4] {
        let StoryImage::Raster { width, pixels, .. } = image;
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
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
        // Left half: a local file (red). Right half: an embedded image (blue).
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
                <image href="{}" width="10" height="10"/>
                <image x="10" href="data:image/png;base64,{}" width="10" height="10"/>
            </svg>"#,
            png_path.display(),
            encode(&png_bytes)
        );

        // What iced would do with it: the local file is loaded
        let unchecked = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).unwrap();
        let unchecked = unchecked.to_string(&usvg::WriteOptions::default());
        assert_eq!(unchecked.matches("data:image/png").count(), 2);

        // Drawn here: the local file's half stays empty
        let drawn = render_svg(svg.as_bytes()).unwrap();
        assert_eq!(pixel(&drawn, 5, 5)[3], 0, "the local file was drawn");
        assert_eq!(pixel(&drawn, 15, 5), [0, 0, 255, 255], "the embedded image is missing");

        // A named pipe would block the reader forever; it's never opened
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
            let _ = tx.send(render_svg(svg.as_bytes()).is_ok());
        });
        let finished = rx.recv_timeout(Duration::from_secs(10));
        if finished.is_err() {
            // Unblock the stuck thread before failing
            let _ = std::fs::OpenOptions::new().write(true).open(&pipe);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(finished, Ok(true), "drawing an SVG that names a pipe must not hang");
    }

    /// SVGs come out as ordinary images within the photo size limits, text
    /// included, and huge ones are scaled down.
    #[test]
    fn story_svgs_become_images() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="1000">
            <rect width="4000" height="1000" fill="white"/>
            <text x="10" y="900" font-size="900" font-family="sans-serif" fill="black">Hello</text>
        </svg>"#;
        let drawn = render_svg(svg).unwrap();
        let StoryImage::Raster { width, height, ref pixels } = drawn;
        assert_eq!((width, height), (MAX_IMAGE_WIDTH, 400));
        // Some of it is the black text
        assert!(pixels.chunks(4).any(|p| p[0] < 50 && p[3] == 255), "no text was drawn");
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
        let public = [
            "93.184.215.14", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "100.128.0.1", "64:ff9b::808:808",
            "172.32.0.1", "192.0.1.1", "192.0.0.9", "192.31.196.1", "192.175.48.1", "223.255.255.255",
            "2001:4860:4860::8888", "2001:1::1", "2001:4:112::1", "2001:20::1", "2620:4f:8000::1", "2a00:1450::1",
        ];
        let local = [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254",
            "0.0.0.0", "100.64.0.1", "100.127.255.255", "255.255.255.255", "224.0.0.1",
            "::1", "::", "fe80::1", "fd12:3456::1", "::ffff:192.168.1.1", "::ffff:127.0.0.1",
            "64:ff9b::c0a8:101", "64:ff9b::7f00:1",
            // In the IANA registries, but missed before
            "192.0.0.1", "192.0.0.170", "192.88.99.1", "240.0.0.1", "198.19.255.255",
            "2001:db8::1", "100::1", "2002:c0a8:101::1", "2001::c0a8:101", "2001:2::1", "2001:10::1",
            "64:ff9b:1::c0a8:101", "3fff::1", "5f00::1", "::192.168.1.1", "ff02::1", "fec0::1",
        ];
        for ip in public {
            assert!(is_public_ip(ip.parse().unwrap()), "{} should be public", ip);
        }
        for ip in local {
            assert!(!is_public_ip(ip.parse().unwrap()), "{} should be local", ip);
        }
        let allowed = |u: &str| public_url_allowed(&Url::parse(u).unwrap());
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

    /// Which addresses count as deliberately local feeds.
    #[test]
    fn local_feed_addresses() {
        let local = |u: &str| is_local_address(&Url::parse(u).unwrap());
        for u in [
            "http://localhost:8080/feed", "http://nas/rss", "http://192.168.1.5/feed", "http://[::1]/f",
            "http://pi.local/feed", "http://server.lan/rss", "http://x.home.arpa/f", "http://feeds.internal/",
            "http://NAS.LOCAL./feed",
        ] {
            assert!(local(u), "{} should be local", u);
        }
        for u in ["https://example.com/feed", "http://8.8.8.8/x", "https://blog.localhost-tools.com/rss", "https://lan.example.org/"] {
            assert!(!local(u), "{} should be public", u);
        }
    }

    /// A public download can't be redirected, or looked up, to this machine.
    #[tokio::test]
    async fn public_downloads_cant_be_steered_to_local_addresses() {
        let base = test_server().await;
        let client = public_client(true);
        // A redirect to a numeric local address
        let redirected = client.get(format!("{}/redirect-to/{}/good", base, base)).send().await;
        let error = format!("{:?}", redirected.unwrap_err());
        assert!(error.contains("redirected to a local"), "{}", error);
        // A name that resolves locally
        let named = client.get(base.replace("127.0.0.1", "localhost")).send().await;
        let error = format!("{:?}", named.unwrap_err());
        assert!(error.contains("isn't a public internet address"), "{}", error);
    }

    /// A feed can add at most `MAX_NEW_PER_REFRESH` stories per refresh and
    /// `MAX_NEW_PER_DAY` per day; held-back stories come in later.
    #[tokio::test]
    async fn feeds_cant_flood_the_archive() {
        let base = test_server().await;
        let db = Database::open_in_memory().unwrap();
        let fetcher = FeedFetcher::new();
        let url = format!("{}/many", base); // 1500 stories
        let feed = db.add_or_update_feed(&url, "Many", None, None, None).unwrap();

        let first = fetcher.fetch_and_sync(&db, feed).await;
        assert_eq!((first.articles_inserted, first.limited), (MAX_NEW_PER_REFRESH, true), "{:?}", first.error);
        let second = fetcher.fetch_and_sync(&db, feed).await;
        assert_eq!((second.articles_inserted, second.limited), (500, false));
        assert_eq!(second.articles_unchanged, 1000);

        // Daily allowance: a second feed that has nearly used up today's
        let other = db.add_or_update_feed(&format!("{}/many?2", base), "Many 2", None, None, None).unwrap();
        let today = Utc::now().format("%Y-%m-%d").to_string();
        db.add_intake(other, &today, MAX_NEW_PER_DAY - 10, 0).unwrap();
        let capped = fetcher.fetch_and_sync(&db, other).await;
        assert_eq!((capped.articles_inserted, capped.limited), (10, true));
        // Yesterday's intake doesn't count
        db.add_intake(other, "2000-01-01", 0, 0).unwrap();
        let next_day = fetcher.fetch_and_sync(&db, other).await;
        assert_eq!(next_day.articles_inserted, MAX_NEW_PER_REFRESH);
    }

    /// "Blue Origin will launch Google&#8217;s AI server satellites": The
    /// Verge's titles are HTML, and sometimes use character references.
    #[test]
    fn titles_are_read_as_their_feed_writes_them() {
        let atom = br#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom">
            <title type="text">The Verge</title><id>v</id><updated>2026-11-19T13:53:38Z</updated>
            <entry><title type="html"><![CDATA[Blue Origin will launch Google&#8217;s AI server satellites in 2029]]></title>
              <id>a</id><updated>2026-11-19T13:53:38Z</updated></entry>
            <entry><title type="html">Snakes &amp;#038; Ladders &lt;em&gt;live&lt;/em&gt;&lt;!-- x --&gt; &amp;lt;3</title>
              <id>b</id><updated>2026-11-19T13:53:38Z</updated></entry>
            <entry><title type="text">x &lt;b&gt; y &amp;amp; z</title><id>c</id><updated>2026-11-19T13:53:38Z</updated></entry>
        </feed>"#;
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Kagi&amp;nbsp;Small Web</title>
            <item><title>Why &lt;iframe&gt; is risky &amp;hellip;</title><guid>d</guid></item>
            <item><title>Comcast Is Just Cable, AT&amp;amp;T Says</title><guid>e</guid></item>
        </channel></rss>"#;
        let titles = |bytes: &[u8]| {
            let feed = parse_feed(bytes).unwrap();
            let mut all = vec![feed_line(
                &feed.title.as_ref().unwrap().content,
                Markup::of_title(feed.title.as_ref().unwrap(), &feed.feed_type),
                MAX_TITLE_CHARS,
            )];
            for entry in &feed.entries {
                let title = entry.title.as_ref().unwrap();
                all.push(feed_line(&title.content, Markup::of_title(title, &feed.feed_type), MAX_TITLE_CHARS));
            }
            all
        };
        assert_eq!(
            titles(atom),
            [
                "The Verge",
                "Blue Origin will launch Google’s AI server satellites in 2029",
                "Snakes & Ladders live <3", // HTML: tags and comments dropped, then decoded
                "x <b> y &amp; z",          // plain text: shown as sent
            ]
        );
        assert_eq!(
            titles(rss),
            [
                "Kagi Small Web",
                "Why <iframe> is risky …", // RSS: a "<" stays text
                "Comcast Is Just Cable, AT&T Says",
            ]
        );
    }

    /// Character references are read as the HTML standard says, browsers'
    /// quirks included.
    #[test]
    fn html_character_references_follow_the_standard() {
        let read = |text: &str| feed_line(text, Markup::References, 200);
        assert_eq!(read("It&rsquo;s &ldquo;gatekeeping&rdquo;&hellip;"), "It’s “gatekeeping”…");
        assert_eq!(read("&#x2014; 5&#8211;7 &NotEqualTilde;"), "— 5–7 ≂̸");
        // Old Windows numbers are Windows-1252 characters, not controls
        assert_eq!(read("It&#146;s &#147;quoted&#148; &#150; &#128;5"), "It’s “quoted” – €5");
        // Without the ";": the old names and numbers still count
        assert_eq!(read("Fish &amp chips &copy 2026, &#8217s"), "Fish & chips © 2026, ’s");
        // Not references: left alone
        assert_eq!(read("AT&T & Verizon; R&D; Q&A trailing &"), "AT&T & Verizon; R&D; Q&A trailing &");
        assert_eq!(read("&madeup; &#; &#xZZ;"), "&madeup; &#; &#xZZ;");
        // NUL and numbers that aren't characters: the replacement character
        assert_eq!(read("a&#0;b&#xD800;c&#1114112;d"), "a\u{FFFD}b\u{FFFD}c\u{FFFD}d");
        // A browser's quirk: "&not" needs no ";", so this is "¬"
        assert_eq!(read("&notit;"), "¬it;");
        // No-break spaces become spaces, so a title can wrap
        assert_eq!(read("So&nbsp;many&nbsp;years"), "So many years");
        // HTML: a bare "<" before a space is text; "<y" starts a tag, as in a browser
        assert_eq!(feed_line("x < y and z", Markup::Html, 200), "x < y and z");
        assert_eq!(feed_line("x <y and z", Markup::Html, 200), "x");
        assert_eq!(feed_line("a <b title=\"1 > 0\">bold</b> c", Markup::Html, 200), "a bold c");
        // A decoded control or direction override is cleaned like any other
        assert_eq!(read("Invoice for &#8238;fdp.exe&#10;x"), "Invoice for fdp.exe x");
        // Authors keep an address in angle brackets
        assert_eq!(read("Pat O&#8217;Brien <pat@example.com>"), "Pat O’Brien <pat@example.com>");
    }

    #[test]
    fn oversized_text_is_cut() {
        assert_eq!(clean_line("A\ntitle\u{1b}[31m red", 100), "A title [31m red");
        // "Invoice for exe.pdf" shown reversed by an override; removed. Real
        // right-to-left text is untouched.
        assert_eq!(clean_line("Invoice for \u{202E}fdp.exe", 100), "Invoice for fdp.exe");
        assert_eq!(clean_line("\u{2067}שלום\u{2069} world", 100), "שלום world");
        assert_eq!(clean_line("مرحبا بالعالم", 100), "مرحبا بالعالم");
        let long = "é".repeat(2000);
        let cut = clean_line(&long, MAX_TITLE_CHARS);
        assert_eq!(cut.chars().count(), MAX_TITLE_CHARS + 1); // plus "…"
        let story = "<p>".to_string() + &"é".repeat(MAX_STORY_BYTES);
        let capped = cap_story(story);
        assert!(capped.len() < MAX_STORY_BYTES + 300);
        assert!(capped.ends_with("Open it in the browser for the rest.]</em></p>"));
        assert_eq!(cap_story("short".into()), "short");
    }

    /// An image declaring an enormous size is refused before decoding.
    #[test]
    fn decompression_bombs_are_refused() {
        // A real 1x1 PNG, then its header rewritten to claim 100000x100000
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        png[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let error = decode_image(&png).unwrap_err();
        assert!(error.contains("too large"), "{}", error);
        // A normal image still decodes
        let mut ok = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 2))
            .write_to(&mut std::io::Cursor::new(&mut ok), image::ImageOutputFormat::Png)
            .unwrap();
        assert!(matches!(decode_image(&ok), Ok(StoryImage::Raster { width: 3, height: 2, .. })));
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
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
                    let gzip = |body: &[u8]| {
                        use std::io::Write;
                        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                        encoder.write_all(body).unwrap();
                        encoder.finish().unwrap()
                    };
                    let accepts_gzip = request
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("accept-encoding:") && l.contains("gzip"));
                    match path.as_str() {
                        // Only to clients that ask for it, as servers do
                        "/gzip" if accepts_gzip => {
                            let body = gzip(feed.as_bytes());
                            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = sock.write_all(header.as_bytes()).await;
                            let _ = sock.write_all(&body).await;
                        }
                        "/gzip" => {
                            let _ = sock.write_all(b"HTTP/1.1 406 Not Acceptable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        }
                        // 26 MB of feed in about 26 KB: over the limit once unpacked
                        "/gzip-bomb" => {
                            let body = gzip(&vec![b' '; MAX_FEED_BYTES + 1024 * 1024]);
                            let header = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = sock.write_all(header.as_bytes()).await;
                            let _ = sock.write_all(&body).await;
                        }
                        "/good" => {
                            let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", feed.len(), feed).as_bytes()).await;
                        }
                        p if p.starts_with("/many") => {
                            let items: String = (0..1500)
                                .map(|i| format!("<item><guid>m{}</guid><title>Story {}</title><description>x</description></item>", i, i))
                                .collect();
                            let body = format!(r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Many</title>{}</channel></rss>"#, items);
                            let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await;
                        }
                        p if p.starts_with("/redirect-to/") => {
                            let target = &p["/redirect-to/".len()..];
                            let _ = sock.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target).as_bytes()).await;
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

    /// Feeds are asked for compressed and unpacked as they arrive; the size
    /// limit applies to the unpacked feed, so a small download can't
    /// unpack into gigabytes.
    #[tokio::test]
    async fn compressed_feeds_are_unpacked_within_the_size_limit() {
        let base = test_server().await;
        let db = Database::open_in_memory().unwrap();
        let fetcher = FeedFetcher::new();
        let gzip = db.add_or_update_feed(&format!("{}/gzip", base), "G", None, None, None).unwrap();
        let summary = fetcher.fetch_and_sync(&db, gzip).await;
        assert_eq!((summary.error.as_deref(), summary.articles_inserted), (None, 2));

        let bomb = db.add_or_update_feed(&format!("{}/gzip-bomb", base), "B", None, None, None).unwrap();
        let error = fetcher.fetch_and_sync(&db, bomb).await.error.unwrap_or_default();
        assert!(error.contains("too large"), "{}", error);
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
