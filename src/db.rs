use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{Article, ArticleFilter, Feed, Folder, IngestionResult};

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl Database {
    /// Opens or creates the SQLite database at standard Linux XDG path
    /// (~/.local/share/rss_reader/rss_reader.db).
    pub fn open_default() -> Result<Self> {
        if let Ok(env_path) = std::env::var("RSS_READER_DB") {
            let path = PathBuf::from(env_path);
            return Self::open(&path);
        }

        if let Ok(path) = Self::default_db_path() {
            if let Ok(db) = Self::open(&path) {
                return Ok(db);
            }
        }

        // Fallback to local directory when XDG directory is not writable
        Self::open(Path::new("rss_reader.db"))
    }

    /// Opens or creates the SQLite database at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory {:?}", parent))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {:?}", path))?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path.to_path_buf(),
        };

        db.init_schema()?;
        Ok(db)
    }

    /// Creates an in-memory database for testing.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
        };
        db.init_schema()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.db_path
    }

    pub fn default_db_path() -> Result<PathBuf> {
        let proj = ProjectDirs::from("org", "rss_reader", "rss_reader")
            .context("Could not determine user XDG data directory")?;
        let data_dir = proj.data_dir();
        Ok(data_dir.join("rss_reader.db"))
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        // 1. Pragmas for high performance, crash-safety, and integrity
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            "#,
        )?;

        // 2. Core Tables
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS feeds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
                url TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                description TEXT,
                site_url TEXT,
                last_fetched_at TEXT,
                etag TEXT,
                last_modified TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS articles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                guid TEXT,
                guid_hash TEXT NOT NULL,
                title TEXT NOT NULL,
                url TEXT,
                author TEXT,
                summary TEXT,
                content_html TEXT,
                content_text TEXT,
                published_at TEXT,
                updated_at TEXT,
                fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
                is_read INTEGER NOT NULL DEFAULT 0,
                is_starred INTEGER NOT NULL DEFAULT 0,
                UNIQUE(feed_id, guid_hash)
            );

            CREATE INDEX IF NOT EXISTS idx_articles_feed_id ON articles(feed_id);
            CREATE INDEX IF NOT EXISTS idx_articles_published_at ON articles(published_at DESC);
            CREATE INDEX IF NOT EXISTS idx_articles_fetched_at ON articles(fetched_at DESC);
            CREATE INDEX IF NOT EXISTS idx_articles_read_starred ON articles(is_read, is_starred);
            "#,
        )?;

        // Migration: Ensure folder_id exists on feeds if database was created previously
        let _ = conn.execute(
            "ALTER TABLE feeds ADD COLUMN folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_feeds_folder_id ON feeds(folder_id)",
            [],
        );
        // Migration: user-defined feed order within a folder (NULL = not yet
        // arranged; those sort alphabetically after arranged feeds)
        let _ = conn.execute("ALTER TABLE feeds ADD COLUMN sort_order INTEGER", []);

        // 3. Full-Text Search (FTS5) for instant offline search across entire story history
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts USING fts5(
                title,
                summary,
                content_text,
                author,
                content='articles',
                content_rowid='id'
            );

            -- Triggers to keep FTS index synchronized with articles table
            CREATE TRIGGER IF NOT EXISTS trg_articles_ai AFTER INSERT ON articles BEGIN
                INSERT INTO articles_fts(rowid, title, summary, content_text, author)
                VALUES (new.id, new.title, new.summary, new.content_text, new.author);
            END;

            CREATE TRIGGER IF NOT EXISTS trg_articles_ad AFTER DELETE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, summary, content_text, author)
                VALUES('delete', old.id, old.title, old.summary, old.content_text, old.author);
            END;

            CREATE TRIGGER IF NOT EXISTS trg_articles_au AFTER UPDATE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, summary, content_text, author)
                VALUES('delete', old.id, old.title, old.summary, old.content_text, old.author);
                INSERT INTO articles_fts(rowid, title, summary, content_text, author)
                VALUES (new.id, new.title, new.summary, new.content_text, new.author);
            END;
            "#,
        )?;

        Ok(())
    }

    // --- Folder Operations ---

    pub fn create_folder(&self, name: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO folders (name) VALUES (?1)",
            params![name.trim()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_or_create_folder(&self, name: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM folders WHERE name = ?1 COLLATE NOCASE",
                params![name.trim()],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO folders (name) VALUES (?1)",
                params![name.trim()],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }

    pub fn rename_folder(&self, folder_id: i64, new_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE folders SET name = ?1 WHERE id = ?2",
            params![new_name.trim(), folder_id],
        )?;
        Ok(())
    }

    pub fn delete_folder(&self, folder_id: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Its feeds land in Uncategorized: forget their order in this folder so
        // they slot in alphabetically after any feeds arranged there by hand.
        tx.execute(
            "UPDATE feeds SET sort_order = NULL WHERE folder_id = ?1",
            params![folder_id],
        )?;
        // Deleting the folder automatically unlinks feeds (folder_id becomes NULL),
        // preserving all feeds and archived stories.
        tx.execute("DELETE FROM folders WHERE id = ?1", params![folder_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_feed_folder(&self, feed_id: i64, folder_id: Option<i64>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            // Clearing sort_order places the feed after any manually arranged
            // feeds in its new folder (or alphabetically if none are arranged)
            "UPDATE feeds SET folder_id = ?1, sort_order = NULL WHERE id = ?2",
            params![folder_id, feed_id],
        )?;
        Ok(())
    }

    /// Puts `ordered_feed_ids` into `folder_id` (None = Uncategorized) in exactly
    /// this order. Used for drag-and-drop reordering and for moving a feed to a
    /// specific position in another folder.
    pub fn place_feeds(&self, folder_id: Option<i64>, ordered_feed_ids: &[i64]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE feeds SET folder_id = ?1, sort_order = ?2 WHERE id = ?3")?;
            for (position, feed_id) in ordered_feed_ids.iter().enumerate() {
                stmt.execute(params![folder_id, position as i64, feed_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_folders(&self) -> Result<Vec<Folder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                fo.id, fo.name,
                COUNT(DISTINCT fe.id) as feed_count,
                COALESCE(SUM(CASE WHEN a.is_read = 0 THEN 1 ELSE 0 END), 0) as unread_count,
                COALESCE(COUNT(a.id), 0) as total_count
            FROM folders fo
            LEFT JOIN feeds fe ON fe.folder_id = fo.id
            LEFT JOIN articles a ON a.feed_id = fe.id
            GROUP BY fo.id, fo.name
            ORDER BY fo.name COLLATE NOCASE ASC
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(Folder {
                id: row.get(0)?,
                name: row.get(1)?,
                feed_count: row.get(2)?,
                unread_count: row.get(3)?,
                total_count: row.get(4)?,
            })
        })?;

        let mut folders = Vec::new();
        for f in rows {
            folders.push(f?);
        }
        Ok(folders)
    }

    // --- Feed Operations ---

    pub fn add_or_update_feed(
        &self,
        url: &str,
        title: &str,
        description: Option<&str>,
        site_url: Option<&str>,
        folder_id: Option<i64>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO feeds (url, title, description, site_url, folder_id)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(url) DO UPDATE SET
                title = CASE WHEN excluded.title != '' THEN excluded.title ELSE feeds.title END,
                description = COALESCE(excluded.description, feeds.description),
                site_url = COALESCE(excluded.site_url, feeds.site_url),
                folder_id = COALESCE(excluded.folder_id, feeds.folder_id)
            "#,
            params![url, title, description, site_url, folder_id],
        )?;

        let id = conn.query_row(
            "SELECT id FROM feeds WHERE url = ?1",
            params![url],
            |row| row.get(0),
        )?;

        Ok(id)
    }

    pub fn update_feed_http_meta(
        &self,
        feed_id: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fetched_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let fetched_at_str = fetched_at.to_rfc3339();
        conn.execute(
            r#"
            UPDATE feeds
            SET etag = ?1,
                last_modified = ?2,
                last_fetched_at = ?3
            WHERE id = ?4
            "#,
            params![etag, last_modified, fetched_at_str, feed_id],
        )?;
        Ok(())
    }

    pub fn get_feeds(&self) -> Result<Vec<Feed>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                f.id, f.url, f.title, f.description, f.site_url,
                f.last_fetched_at, f.etag, f.last_modified, f.created_at,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.is_read = 0) as unread_count,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id) as total_count,
                f.folder_id
            FROM feeds f
            ORDER BY f.sort_order IS NULL, f.sort_order, f.title COLLATE NOCASE ASC
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            let last_fetched_str: Option<String> = row.get(5)?;
            let created_at_str: String = row.get(8)?;

            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                site_url: row.get(4)?,
                last_fetched_at: last_fetched_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                etag: row.get(6)?,
                last_modified: row.get(7)?,
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                unread_count: row.get(9)?,
                total_count: row.get(10)?,
                folder_id: row.get(11)?,
            })
        })?;

        let mut feeds = Vec::new();
        for feed in rows {
            feeds.push(feed?);
        }
        Ok(feeds)
    }

    pub fn get_feed(&self, id: i64) -> Result<Option<Feed>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                f.id, f.url, f.title, f.description, f.site_url,
                f.last_fetched_at, f.etag, f.last_modified, f.created_at,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.is_read = 0) as unread_count,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id) as total_count,
                f.folder_id
            FROM feeds f
            WHERE f.id = ?1
            "#,
        )?;

        let result = stmt.query_row(params![id], |row| {
            let last_fetched_str: Option<String> = row.get(5)?;
            let created_at_str: String = row.get(8)?;

            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                site_url: row.get(4)?,
                last_fetched_at: last_fetched_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                etag: row.get(6)?,
                last_modified: row.get(7)?,
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                unread_count: row.get(9)?,
                total_count: row.get(10)?,
                folder_id: row.get(11)?,
            })
        }).optional()?;

        Ok(result)
    }

    /// Sets a custom display name. Sync keeps it, since it only replaces titles
    /// that are empty or equal to the feed URL.
    pub fn rename_feed(&self, feed_id: i64, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE feeds SET title = ?1 WHERE id = ?2", params![title, feed_id])?;
        Ok(())
    }

    /// Updates a feed's name and URL. When the URL changes, the HTTP caching
    /// headers are cleared so the next refresh does a full fetch of the new URL.
    /// Archived stories stay attached to the feed either way.
    pub fn update_feed_details(&self, feed_id: i64, title: &str, url: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            UPDATE feeds
            SET title = ?1,
                etag            = CASE WHEN url = ?2 THEN etag ELSE NULL END,
                last_modified   = CASE WHEN url = ?2 THEN last_modified ELSE NULL END,
                last_fetched_at = CASE WHEN url = ?2 THEN last_fetched_at ELSE NULL END,
                url = ?2
            WHERE id = ?3
            "#,
            params![title, url, feed_id],
        )?;
        Ok(())
    }

    pub fn delete_feed(&self, feed_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM feeds WHERE id = ?1", params![feed_id])?;
        Ok(())
    }

    // --- Article Archival & Ingestion ---

    /// Inserts or updates an article without ever losing existing articles or resetting read/starred states.
    /// This is the core engine guarantee: newly ingested feed items never purge older items.
    pub fn upsert_article(
        &self,
        feed_id: i64,
        guid: Option<&str>,
        guid_hash: &str,
        title: &str,
        url: Option<&str>,
        author: Option<&str>,
        summary: Option<&str>,
        content_html: Option<&str>,
        content_text: Option<&str>,
        published_at: Option<DateTime<Utc>>,
        updated_at: Option<DateTime<Utc>>,
    ) -> Result<IngestionResult> {
        let conn = self.conn.lock().unwrap();

        let pub_str = published_at.map(|dt| dt.to_rfc3339());
        let upd_str = updated_at.map(|dt| dt.to_rfc3339());
        let fetched_now = Utc::now().to_rfc3339();

        // Check if article already exists
        let existing: Option<(i64, Option<String>, Option<String>)> = conn.query_row(
            "SELECT id, title, summary FROM articles WHERE feed_id = ?1 AND guid_hash = ?2",
            params![feed_id, guid_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;

        match existing {
            None => {
                conn.execute(
                    r#"
                    INSERT INTO articles (
                        feed_id, guid, guid_hash, title, url, author, summary,
                        content_html, content_text, published_at, updated_at, fetched_at,
                        is_read, is_starred
                    ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, 0
                    )
                    "#,
                    params![
                        feed_id, guid, guid_hash, title, url, author, summary,
                        content_html, content_text, pub_str, upd_str, fetched_now
                    ],
                )?;
                Ok(IngestionResult::Inserted)
            }
            Some((article_id, old_title, old_summary)) => {
                let title_changed = old_title.as_deref() != Some(title);
                let summary_changed = old_summary.as_deref() != summary;

                if title_changed || summary_changed || updated_at.is_some() {
                    // Update content while carefully preserving read & starred statuses
                    conn.execute(
                        r#"
                        UPDATE articles SET
                            title = ?1,
                            url = COALESCE(?2, url),
                            author = COALESCE(?3, author),
                            summary = COALESCE(?4, summary),
                            content_html = COALESCE(?5, content_html),
                            content_text = COALESCE(?6, content_text),
                            updated_at = COALESCE(?7, updated_at)
                        WHERE id = ?8
                        "#,
                        params![
                            title, url, author, summary, content_html, content_text, upd_str, article_id
                        ],
                    )?;
                    Ok(IngestionResult::Updated)
                } else {
                    Ok(IngestionResult::Unchanged)
                }
            }
        }
    }

    pub fn get_articles(&self, filter: &ArticleFilter) -> Result<Vec<Article>> {
        let conn = self.conn.lock().unwrap();

        let mut query = String::from(
            r#"
            SELECT 
                a.id, a.feed_id, f.title as feed_title, a.guid, a.guid_hash,
                a.title, a.url, a.author, a.summary, a.content_html, a.content_text,
                a.published_at, a.updated_at, a.fetched_at, a.is_read, a.is_starred
            FROM articles a
            JOIN feeds f ON f.id = a.feed_id
            WHERE 1=1
            "#,
        );

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(feed_id) = filter.feed_id {
            query.push_str(" AND a.feed_id = ?");
            params_vec.push(Box::new(feed_id));
        }

        if let Some(folder_id) = filter.folder_id {
            query.push_str(" AND a.feed_id IN (SELECT id FROM feeds WHERE folder_id = ?)");
            params_vec.push(Box::new(folder_id));
        }

        if filter.unread_only {
            query.push_str(" AND a.is_read = 0");
        }

        if filter.starred_only {
            query.push_str(" AND a.is_starred = 1");
        }

        if let Some(ref search) = filter.search_query {
            if !search.trim().is_empty() {
                query.push_str(" AND a.id IN (SELECT rowid FROM articles_fts WHERE articles_fts MATCH ?)");
                // Format FTS5 search query nicely (e.g. quote words)
                let fts_query = format!("\"{}\"", search.replace('"', "\"\""));
                params_vec.push(Box::new(fts_query));
            }
        }

        query.push_str(" ORDER BY COALESCE(a.published_at, a.fetched_at) DESC");

        if let Some(limit) = filter.limit {
            query.push_str(&format!(" LIMIT {}", limit));
            if let Some(offset) = filter.offset {
                query.push_str(&format!(" OFFSET {}", offset));
            }
        }

        let mut stmt = conn.prepare(&query)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let pub_str: Option<String> = row.get(11)?;
            let upd_str: Option<String> = row.get(12)?;
            let fet_str: String = row.get(13)?;

            Ok(Article {
                id: row.get(0)?,
                feed_id: row.get(1)?,
                feed_title: row.get(2)?,
                guid: row.get(3)?,
                guid_hash: row.get(4)?,
                title: row.get(5)?,
                url: row.get(6)?,
                author: row.get(7)?,
                summary: row.get(8)?,
                content_html: row.get(9)?,
                content_text: row.get(10)?,
                published_at: pub_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                updated_at: upd_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                fetched_at: DateTime::parse_from_rfc3339(&fet_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                is_read: row.get::<_, i64>(14)? != 0,
                is_starred: row.get::<_, i64>(15)? != 0,
            })
        })?;

        let mut articles = Vec::new();
        for article in rows {
            articles.push(article?);
        }
        Ok(articles)
    }

    pub fn get_article(&self, id: i64) -> Result<Option<Article>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                a.id, a.feed_id, f.title as feed_title, a.guid, a.guid_hash,
                a.title, a.url, a.author, a.summary, a.content_html, a.content_text,
                a.published_at, a.updated_at, a.fetched_at, a.is_read, a.is_starred
            FROM articles a
            JOIN feeds f ON f.id = a.feed_id
            WHERE a.id = ?1
            "#,
        )?;

        let result = stmt.query_row(params![id], |row| {
            let pub_str: Option<String> = row.get(11)?;
            let upd_str: Option<String> = row.get(12)?;
            let fet_str: String = row.get(13)?;

            Ok(Article {
                id: row.get(0)?,
                feed_id: row.get(1)?,
                feed_title: row.get(2)?,
                guid: row.get(3)?,
                guid_hash: row.get(4)?,
                title: row.get(5)?,
                url: row.get(6)?,
                author: row.get(7)?,
                summary: row.get(8)?,
                content_html: row.get(9)?,
                content_text: row.get(10)?,
                published_at: pub_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                updated_at: upd_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                fetched_at: DateTime::parse_from_rfc3339(&fet_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                is_read: row.get::<_, i64>(14)? != 0,
                is_starred: row.get::<_, i64>(15)? != 0,
            })
        }).optional()?;

        Ok(result)
    }

    pub fn set_article_read(&self, id: i64, is_read: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE articles SET is_read = ?1 WHERE id = ?2",
            params![if is_read { 1 } else { 0 }, id],
        )?;
        Ok(())
    }

    pub fn set_all_articles_read(
        &self,
        feed_id: Option<i64>,
        folder_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        if let Some(feed_id) = feed_id {
            conn.execute(
                "UPDATE articles SET is_read = 1 WHERE feed_id = ?1 AND is_read = 0",
                params![feed_id],
            )?;
        } else if let Some(folder_id) = folder_id {
            conn.execute(
                "UPDATE articles SET is_read = 1 WHERE feed_id IN (SELECT id FROM feeds WHERE folder_id = ?1) AND is_read = 0",
                params![folder_id],
            )?;
        } else {
            conn.execute("UPDATE articles SET is_read = 1 WHERE is_read = 0", [])?;
        }
        Ok(())
    }

    pub fn toggle_article_starred(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let new_val: bool = conn.query_row(
            "UPDATE articles SET is_starred = NOT is_starred WHERE id = ?1 RETURNING is_starred",
            params![id],
            |row| Ok(row.get::<_, i64>(0)? != 0),
        )?;
        Ok(new_val)
    }

    pub fn delete_article(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM articles WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get_total_stats(&self) -> Result<(i64, i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let (total_feeds, total_articles, unread_articles) = conn.query_row(
            r#"
            SELECT 
                (SELECT COUNT(*) FROM feeds),
                (SELECT COUNT(*) FROM articles),
                (SELECT COUNT(*) FROM articles WHERE is_read = 0)
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok((total_feeds, total_articles, unread_articles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feed_and_article_persistence() {
        let db = Database::open_in_memory().unwrap();

        // 1. Add feed
        let feed_id = db.add_or_update_feed(
            "https://example.com/feed.xml",
            "Example Feed",
            Some("An example description"),
            Some("https://example.com"),
            None,
        ).unwrap();

        assert_eq!(feed_id, 1);

        // 2. Insert an article
        let res1 = db.upsert_article(
            feed_id,
            Some("item-1"),
            "hash-item-1",
            "First Story",
            Some("https://example.com/story-1"),
            Some("Alice"),
            Some("Short summary of story 1"),
            Some("<p>Full content of story 1</p>"),
            Some("Full content of story 1"),
            Some(Utc::now()),
            None,
        ).unwrap();
        assert_eq!(res1, IngestionResult::Inserted);

        // 3. Second sync - same item unchanged
        let res2 = db.upsert_article(
            feed_id,
            Some("item-1"),
            "hash-item-1",
            "First Story",
            Some("https://example.com/story-1"),
            Some("Alice"),
            Some("Short summary of story 1"),
            Some("<p>Full content of story 1</p>"),
            Some("Full content of story 1"),
            Some(Utc::now()),
            None,
        ).unwrap();
        assert_eq!(res2, IngestionResult::Unchanged);

        // 4. Mark as read
        let articles = db.get_articles(&ArticleFilter::default()).unwrap();
        assert_eq!(articles.len(), 1);
        assert!(!articles[0].is_read);

        db.set_article_read(articles[0].id, true).unwrap();

        // 5. Simulate feed rolling over: new article arrives, but old article is NOT in the feed anymore
        let res3 = db.upsert_article(
            feed_id,
            Some("item-2"),
            "hash-item-2",
            "Second Story",
            Some("https://example.com/story-2"),
            Some("Bob"),
            Some("Summary 2"),
            Some("<p>Content 2</p>"),
            Some("Content 2"),
            Some(Utc::now()),
            None,
        ).unwrap();
        assert_eq!(res3, IngestionResult::Inserted);

        // Verify BOTH articles are preserved in the database (old story was NOT lost!)
        let all_articles = db.get_articles(&ArticleFilter::default()).unwrap();
        assert_eq!(all_articles.len(), 2);

        // Verify read state of story 1 was preserved
        let story1 = db.get_article(articles[0].id).unwrap().unwrap();
        assert!(story1.is_read);

        // 6. Test FTS5 full-text search
        let filter = ArticleFilter {
            search_query: Some("Alice".to_string()),
            ..Default::default()
        };
        let search_results = db.get_articles(&filter).unwrap();
        assert_eq!(search_results.len(), 1);
        assert_eq!(search_results[0].title, "First Story");
    }

    #[test]
    fn test_folders_and_grouping() {
        let db = Database::open_in_memory().unwrap();

        let tech_id = db.create_folder("Technology").unwrap();
        let news_id = db.create_folder("News").unwrap();

        let feed1 = db.add_or_update_feed("https://rust-lang.org/feed", "Rust Blog", None, None, Some(tech_id)).unwrap();
        let feed2 = db.add_or_update_feed("https://phoronix.com/feed", "Phoronix", None, None, Some(tech_id)).unwrap();
        let feed3 = db.add_or_update_feed("https://news.ycombinator.com/rss", "HN", None, None, Some(news_id)).unwrap();

        // Insert stories in feeds
        db.upsert_article(feed1, Some("r1"), "hash-r1", "Rust 2026", None, None, None, None, None, Some(Utc::now()), None).unwrap();
        db.upsert_article(feed2, Some("p1"), "hash-p1", "Linux 6.18", None, None, None, None, None, Some(Utc::now()), None).unwrap();
        db.upsert_article(feed3, Some("h1"), "hash-h1", "Show HN", None, None, None, None, None, Some(Utc::now()), None).unwrap();

        // Check folder aggregations
        let folders = db.get_folders().unwrap();
        assert_eq!(folders.len(), 2);

        let tech_folder = folders.iter().find(|f| f.id == tech_id).unwrap();
        assert_eq!(tech_folder.name, "Technology");
        assert_eq!(tech_folder.feed_count, 2);
        assert_eq!(tech_folder.unread_count, 2);
        assert_eq!(tech_folder.total_count, 2);

        // Filter articles by folder
        let tech_filter = ArticleFilter {
            folder_id: Some(tech_id),
            ..Default::default()
        };
        let tech_articles = db.get_articles(&tech_filter).unwrap();
        assert_eq!(tech_articles.len(), 2);

        // Mark folder as read
        db.set_all_articles_read(None, Some(tech_id)).unwrap();
        let tech_folders_after = db.get_folders().unwrap();
        let tech_folder_after = tech_folders_after.iter().find(|f| f.id == tech_id).unwrap();
        assert_eq!(tech_folder_after.unread_count, 0);

        // News should still have 1 unread
        let news_folder_after = tech_folders_after.iter().find(|f| f.id == news_id).unwrap();
        assert_eq!(news_folder_after.unread_count, 1);
    }
}
