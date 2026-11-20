use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub feed_count: i64,
    pub unread_count: i64,
    pub total_count: i64,
}

/// A tag in use, as the sidebar lists it (see `shreader_tags`): stories
/// sorted out of their folders, such as promotions
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagView {
    pub id: i64,
    pub name: String,
    pub unread_count: i64,
    pub total_count: i64,
}

#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub folder_id: Option<i64>,
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub site_url: Option<String>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub unread_count: i64,
    pub total_count: i64,
}

#[derive(Debug, Clone)]
pub struct Article {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: Option<String>,
    pub guid: Option<String>,
    pub guid_hash: String,
    pub title: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content_html: Option<String>,
    pub content_text: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
    pub is_read: bool,
    pub is_starred: bool,
    /// The tag it's sorted into, if any (see `shreader_tags::store`)
    pub tag_id: Option<i64>,
}

impl Article {
    /// The date shown for the story: its publication date, but never later
    /// than when it was downloaded. Some feeds date stories in the future
    /// (often a time zone mistake); taken at face value those read "1m ago"
    /// and stay above everything else until the date passes. The story list
    /// is sorted the same way (`Database::get_articles`).
    pub fn shown_date(&self) -> Option<DateTime<Utc>> {
        self.published_at.map(|published| published.min(self.fetched_at))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ArticleFilter {
    pub feed_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub unread_only: bool,
    pub starred_only: bool,
    pub search_query: Option<String>,
    /// Only stories sorted into this tag. Without it, tagged stories are
    /// left out (they're shown in their tag instead), except among starred
    /// stories and when searching everything (no folder, feed or unread
    /// limit), which include them.
    pub tag_id: Option<i64>,
    /// Only this story (to check whether it's part of a view)
    pub article_id: Option<i64>,
    /// Unread stories before read ones (each newest first)
    pub unread_first: bool,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestionResult {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct FeedSyncSummary {
    pub feed_title: String,
    pub articles_inserted: usize,
    pub articles_updated: usize,
    pub articles_unchanged: usize,
    pub error: Option<String>,
    /// Some new stories were held back: the feed sent more than
    /// `feed::MAX_NEW_PER_REFRESH`, or used up its daily allowance
    pub limited: bool,
}
