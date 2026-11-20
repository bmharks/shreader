use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{Article, ArticleFilter, Feed, Folder, IngestionResult, TagView};

/// The archive to open: `new`, after moving an archive from `legacy` there
/// if only `legacy` has one. If it can't be moved (in use, another disk),
/// `legacy` is used where it is, so an archive is never split or lost.
fn adopt_legacy_archive(legacy: &Path, new: &Path) -> PathBuf {
    if new.exists() || !legacy.exists() {
        return new.to_path_buf();
    }
    match move_archive(legacy, new) {
        Ok(()) => {
            tracing::info!("Moved the archive from {:?} to {:?}", legacy, new);
            new.to_path_buf()
        }
        Err(e) => {
            tracing::warn!("Couldn't move the archive from {:?} ({:#}); using it there", legacy, e);
            legacy.to_path_buf()
        }
    }
}

/// Moves a closed SQLite archive from `from` to `to` (a file that doesn't
/// exist yet). Nothing is copied or deleted: it's one rename, after making
/// sure the file alone holds everything.
fn move_archive(from: &Path, to: &Path) -> Result<()> {
    let side_file = |path: &Path, suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    {
        // Fold the write-ahead log into the main file. Closing the last
        // connection then removes the -wal and -shm files.
        let conn = Connection::open(from)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            anyhow::bail!("the archive is in use");
        }
    }
    // Still there: another program (an older copy of the app?) has it open,
    // and would keep writing to the old location
    if side_file(from, "-wal").exists() || side_file(from, "-shm").exists() {
        anyhow::bail!("the archive is open in another program");
    }
    let dir = to.parent().context("no folder for the archive")?;
    std::fs::create_dir_all(dir)?;
    set_private(dir, 0o700);
    std::fs::rename(from, to)?;
    // The old folder, if that left it empty (fails harmlessly otherwise)
    if let Some(old_dir) = from.parent() {
        let _ = std::fs::remove_dir(old_dir);
    }
    Ok(())
}

/// Restricts `path` to `mode` (owner-only permissions). Best effort: on
/// failure the archive still works, just with the default permissions.
fn set_private(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            tracing::warn!("Couldn't make {:?} private: {}", path, e);
        }
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// What `Database::stored_state` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredState {
    New,
    Unchanged,
    Changed,
}

/// How long a query waits for another connection's lock before failing.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl Database {
    /// Opens or creates the archive at the standard Linux location
    /// (~/.local/share/shreader/shreader.db), or at `$SHREADER_DB`
    /// (`$RSS_READER_DB` from before the rename also still works). An
    /// archive at the location used before the rename is moved over first.
    ///
    /// There's deliberately no fallback location: opening an empty database
    /// elsewhere would look like the archive was lost, and anything added
    /// would land in the wrong file.
    pub fn open_default() -> Result<Self> {
        let env_path = std::env::var_os("SHREADER_DB").or_else(|| std::env::var_os("RSS_READER_DB"));
        let path = match env_path {
            Some(env_path) => PathBuf::from(env_path),
            None => {
                let path = match Self::legacy_db_path() {
                    Some(legacy) => adopt_legacy_archive(&legacy, &Self::default_db_path()?),
                    None => Self::default_db_path()?,
                };
                // The app's own folder: private to this user, like the files
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("Failed to create database directory {:?}", dir))?;
                    set_private(dir, 0o700);
                }
                path
            }
        };
        Self::open(&path)
    }

    /// Opens or creates the SQLite database at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory {:?}", parent))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {:?}", path))?;
        // The archive is a reading history: readable by this user only. (New
        // files get the default "everyone can read"; SQLite gives its -wal
        // and -shm files the database file's permissions.)
        set_private(path, 0o600);
        for suffix in ["-wal", "-shm"] {
            let mut side_file = path.as_os_str().to_owned();
            side_file.push(suffix);
            let side_file = PathBuf::from(side_file);
            if side_file.exists() {
                set_private(&side_file, 0o600);
            }
        }
        // Before anything else: switching to WAL below needs a lock too, and
        // without a timeout a moment's lock by another copy of the app fails
        // instantly
        conn.busy_timeout(BUSY_TIMEOUT)?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path.to_path_buf(),
        };

        db.init_schema()
            .with_context(|| format!("Failed to set up the SQLite database at {:?}", path))?;
        Ok(db)
    }

    /// Creates an in-memory database for testing.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
        };
        db.init_schema()?;
        Ok(db)
    }

    /// The connection. A panic elsewhere while it was held "poisons" the
    /// lock; the connection itself is still fine (SQLite rolls back any
    /// unfinished transaction), so carry on rather than make every later
    /// database call panic too.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

        pub fn path(&self) -> &Path {
        &self.db_path
    }

    /// Space the archive takes on disk, in bytes: the database file plus its
    /// write-ahead log (recent changes not yet merged into the file). Story
    /// images aren't stored, so this is everything. 0 for an in-memory database.
    pub fn disk_size(&self) -> u64 {
        let mut wal = self.db_path.clone().into_os_string();
        wal.push("-wal");
        [self.db_path.as_os_str(), wal.as_os_str()]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|meta| meta.len())
            .sum()
    }

    /// ~/.local/share/shreader/shreader.db (named after the app ID,
    /// io.github.bmharks.Shreader)
    pub fn default_db_path() -> Result<PathBuf> {
        let proj = ProjectDirs::from("io.github", "bmharks", "Shreader")
            .context("Could not determine user XDG data directory")?;
        Ok(proj.data_dir().join("shreader.db"))
    }

    /// Where the archive was kept before the app was renamed:
    /// ~/.local/share/rss_reader/rss_reader.db
    fn legacy_db_path() -> Option<PathBuf> {
        ProjectDirs::from("org", "rss_reader", "rss_reader").map(|proj| proj.data_dir().join("rss_reader.db"))
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.lock();

        // 1. Pragmas for high performance, crash-safety, and integrity
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            "#,
        )?;

        // 2. Core Tables
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

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

            -- Feed lookups, and the sidebar's per-feed and per-folder counts
            -- (total and unread), which it answers from the index alone instead
            -- of reading every story. Replaces an index on feed_id by itself.
            CREATE INDEX IF NOT EXISTS idx_articles_feed_read ON articles(feed_id, is_read);
            DROP INDEX IF EXISTS idx_articles_feed_id;
            -- Replaced by the idx_articles_*shown_date indexes below (the list
            -- sorts by a combination of the two dates)
            DROP INDEX IF EXISTS idx_articles_published_at;
            DROP INDEX IF EXISTS idx_articles_fetched_at;
            CREATE INDEX IF NOT EXISTS idx_articles_read_starred ON articles(is_read, is_starred);
            "#,
        )?;

        // The story list's sort order (see `SHOWN_DATE`), so a page of the
        // newest stories is read straight off an index instead of sorting
        // the whole archive. Expression indexes only match a query that
        // spells the expression the same way.
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_articles_shown_date
                ON articles(MIN(COALESCE(published_at, fetched_at), fetched_at));
            -- The Unread view, and "unread first" (read status, then newest
            -- first). Replaces an index without the directions, which only
            -- served the Unread view.
            DROP INDEX IF EXISTS idx_articles_unread_shown_date;
            CREATE INDEX IF NOT EXISTS idx_articles_read_shown_date
                ON articles(is_read, MIN(COALESCE(published_at, fetched_at), fetched_at) DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_articles_starred_shown_date
                ON articles(MIN(COALESCE(published_at, fetched_at), fetched_at)) WHERE is_starred = 1;
            -- Feed and folder views: lets a big folder be sorted from the
            -- index rather than by reading all its stories
            CREATE INDEX IF NOT EXISTS idx_articles_feed_shown_date
                ON articles(feed_id, MIN(COALESCE(published_at, fetched_at), fetched_at));
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
        // Migration: user-defined folder order (NULL = not yet arranged; those
        // sort alphabetically after arranged folders)
        let _ = conn.execute("ALTER TABLE folders ADD COLUMN sort_order INTEGER", []);
        // Migration: how much each feed has added today (UTC), for the daily
        // allowance that stops one feed from filling the disk
        let _ = conn.execute("ALTER TABLE feeds ADD COLUMN intake_day TEXT", []);
        let _ = conn.execute("ALTER TABLE feeds ADD COLUMN intake_stories INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE feeds ADD COLUMN intake_bytes INTEGER NOT NULL DEFAULT 0", []);

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

            -- Only text changes touch the search index; marking stories read
            -- or starred mustn't re-index them. Recreated on every start so
            -- databases made with the older any-column trigger get this one.
            DROP TRIGGER IF EXISTS trg_articles_au;
            CREATE TRIGGER trg_articles_au
            AFTER UPDATE OF title, summary, content_text, author ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, summary, content_text, author)
                VALUES('delete', old.id, old.title, old.summary, old.content_text, old.author);
                INSERT INTO articles_fts(rowid, title, summary, content_text, author)
                VALUES (new.id, new.title, new.summary, new.content_text, new.author);
            END;
            "#,
        )?;

        // 4. Tags (see the shreader-tags crate): which stories are sorted
        // out of their folders into tag views, and what the trainer learned
        shreader_tags::store::ensure_schema(&conn)?;

        // 5. Once: titles stored before HTML character references were
        // decoded ("Google&#8217;s"), as they're stored now
        let done: Option<String> = conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![TITLES_DECODED_SETTING], |r| r.get(0))
            .optional()?;
        if done.is_none() {
            let tx = conn.unchecked_transaction()?;
            let fixed = decode_stored_titles(&tx)?;
            tx.execute("INSERT INTO settings (key, value) VALUES (?1, 'true')", params![TITLES_DECODED_SETTING])?;
            tx.commit()?;
            if fixed > 0 {
                tracing::info!("Decoded HTML character references in {} story titles", fixed);
            }
        }

        Ok(())
    }

    /// The connection, for the tags crate's functions (see
    /// `shreader_tags::store`). Held while `f` runs.
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.lock())
    }

    // --- Folder Operations ---

    /// A saved UI setting, if any.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let value = conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn create_folder(&self, name: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO folders (name) VALUES (?1)",
            params![name.trim()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_or_create_folder(&self, name: &str) -> Result<i64> {
        let conn = self.lock();
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
        let conn = self.lock();
        conn.execute(
            "UPDATE folders SET name = ?1 WHERE id = ?2",
            params![new_name.trim(), folder_id],
        )?;
        Ok(())
    }

    pub fn delete_folder(&self, folder_id: i64) -> Result<()> {
        let mut conn = self.lock();
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
        let conn = self.lock();
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
        let mut conn = self.lock();
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

    /// Puts the folders in exactly this order (drag-and-drop in the sidebar).
    pub fn order_folders(&self, ordered_folder_ids: &[i64]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE folders SET sort_order = ?1 WHERE id = ?2")?;
            for (position, folder_id) in ordered_folder_ids.iter().enumerate() {
                stmt.execute(params![position as i64, folder_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_folders(&self) -> Result<Vec<Folder>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                fo.id, fo.name,
                COUNT(DISTINCT fe.id) as feed_count,
                COALESCE(SUM(CASE WHEN a.is_read = 0 THEN 1 ELSE 0 END), 0) as unread_count,
                COALESCE(COUNT(a.id), 0) as total_count
            FROM folders fo
            LEFT JOIN feeds fe ON fe.folder_id = fo.id
            LEFT JOIN articles a ON a.feed_id = fe.id AND NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)
            GROUP BY fo.id, fo.name
            ORDER BY fo.sort_order IS NULL, fo.sort_order, fo.name COLLATE NOCASE ASC
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
        let conn = self.lock();
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

    /// Stores what a refresh learned about a feed, without touching what the
    /// user controls (folder, order, and a title they chose). The title is
    /// only filled in while it's still the URL placeholder or empty. Returns
    /// the feed's title, or `None` if the feed no longer exists.
    pub fn update_feed_from_fetch(
        &self,
        feed_id: i64,
        parsed_title: Option<&str>,
        description: Option<&str>,
        site_url: Option<&str>,
    ) -> Result<Option<String>> {
        let conn = self.lock();
        let changed = conn.execute(
            r#"
            UPDATE feeds SET
                title = CASE WHEN (title = '' OR title = url) AND ?1 IS NOT NULL THEN ?1 ELSE title END,
                description = COALESCE(?2, description),
                site_url = COALESCE(?3, site_url)
            WHERE id = ?4
            "#,
            params![parsed_title, description, site_url, feed_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let title = conn.query_row("SELECT title FROM feeds WHERE id = ?1", params![feed_id], |row| row.get(0))?;
        Ok(Some(title))
    }

    pub fn update_feed_http_meta(
        &self,
        feed_id: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fetched_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock();
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
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                f.id, f.url, f.title, f.description, f.site_url,
                f.last_fetched_at, f.etag, f.last_modified,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.is_read = 0 AND NOT EXISTS
                    (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)) as unread_count,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND NOT EXISTS
                    (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)) as total_count,
                f.folder_id
            FROM feeds f
            ORDER BY f.sort_order IS NULL, f.sort_order, f.title COLLATE NOCASE ASC
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            let last_fetched_str: Option<String> = row.get(5)?;

            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                site_url: row.get(4)?,
                last_fetched_at: last_fetched_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                etag: row.get(6)?,
                last_modified: row.get(7)?,
                unread_count: row.get(8)?,
                total_count: row.get(9)?,
                folder_id: row.get(10)?,
            })
        })?;

        let mut feeds = Vec::new();
        for feed in rows {
            feeds.push(feed?);
        }
        Ok(feeds)
    }

    pub fn get_feed(&self, id: i64) -> Result<Option<Feed>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                f.id, f.url, f.title, f.description, f.site_url,
                f.last_fetched_at, f.etag, f.last_modified,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.is_read = 0 AND NOT EXISTS
                    (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)) as unread_count,
                (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND NOT EXISTS
                    (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)) as total_count,
                f.folder_id
            FROM feeds f
            WHERE f.id = ?1
            "#,
        )?;

        let result = stmt.query_row(params![id], |row| {
            let last_fetched_str: Option<String> = row.get(5)?;

            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                site_url: row.get(4)?,
                last_fetched_at: last_fetched_str.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|dt| dt.with_timezone(&Utc))),
                etag: row.get(6)?,
                last_modified: row.get(7)?,
                unread_count: row.get(8)?,
                total_count: row.get(9)?,
                folder_id: row.get(10)?,
            })
        }).optional()?;

        Ok(result)
    }

    /// Sets a custom display name. Sync keeps it, since it only replaces titles
    /// that are empty or equal to the feed URL.
    pub fn rename_feed(&self, feed_id: i64, title: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE feeds SET title = ?1 WHERE id = ?2", params![title, feed_id])?;
        Ok(())
    }

    /// Updates a feed's name and URL. When the URL changes, the HTTP caching
    /// headers are cleared so the next refresh does a full fetch of the new URL.
    /// Archived stories stay attached to the feed either way.
    pub fn update_feed_details(&self, feed_id: i64, title: &str, url: &str) -> Result<()> {
        let conn = self.lock();
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

    /// Number of starred stories.
    pub fn starred_count(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM articles WHERE is_starred = 1", [], |r| r.get(0))?)
    }

    /// Deletes archived stories (all, or all but the starred ones), keeping
    /// feeds and folders. Returns how many were deleted.
    ///
    /// Feeds' HTTP caching info is reset so the next refresh downloads each
    /// feed in full: otherwise an unchanged feed answers "304 Not Modified"
    /// and its current stories wouldn't come back until it next posts.
    pub fn clear_stories(&self, keep_starred: bool) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // The delete trigger also removes each story from the search index
        let deleted = if keep_starred {
            tx.execute("DELETE FROM articles WHERE is_starred = 0", [])?
        } else {
            tx.execute("DELETE FROM articles", [])?
        };
        tx.execute(
            "UPDATE feeds SET etag = NULL, last_modified = NULL, last_fetched_at = NULL",
            [],
        )?;
        // FTS5 records deletions as markers instead of freeing their space
        // (~30 MB for a few thousand stories); rebuild it from what's left
        tx.execute("INSERT INTO articles_fts(articles_fts) VALUES('rebuild')", [])?;
        tx.commit()?;
        // Outside the transaction: VACUUM can't run inside one. In WAL mode the
        // compacted pages land in the -wal file, so checkpoint and truncate it
        // too; otherwise neither file shrinks until a clean shutdown.
        conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(deleted)
    }

    pub fn delete_feed(&self, feed_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM feeds WHERE id = ?1", params![feed_id])?;
        Ok(())
    }

    // --- Article Archival & Ingestion ---

    /// Whether a story is already stored, and if so whether its text
    /// (title, summary, content) differs: the same comparison
    /// `upsert_article` makes. Lets a refresh skip converting unchanged
    /// stories to plain text, and check new ones against the allowance.
    pub fn stored_state(
        &self,
        feed_id: i64,
        guid_hash: &str,
        title: &str,
        summary: Option<&str>,
        content_html: Option<&str>,
    ) -> Result<StoredState> {
        let conn = self.lock();
        let changed: Option<bool> = conn
            .query_row(
                r#"
                SELECT NOT (title IS ?3 AND summary IS ?4 AND (?5 IS NULL OR content_html IS ?5))
                FROM articles WHERE feed_id = ?1 AND guid_hash = ?2
                "#,
                params![feed_id, guid_hash, title, summary, content_html],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match changed {
            None => StoredState::New,
            Some(false) => StoredState::Unchanged,
            Some(true) => StoredState::Changed,
        })
    }

    /// New stories and bytes `feed_id` has added on `day` (a UTC date).
    pub fn intake(&self, feed_id: i64, day: &str) -> Result<(u64, u64)> {
        let conn = self.lock();
        let used = conn
            .query_row(
                "SELECT intake_stories, intake_bytes FROM feeds WHERE id = ?1 AND intake_day = ?2",
                params![feed_id, day],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?;
        Ok(used.unwrap_or((0, 0)))
    }

    /// Adds to `feed_id`'s intake for `day`, starting over on a new day.
    pub fn add_intake(&self, feed_id: i64, day: &str, stories: u64, bytes: u64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"
            UPDATE feeds SET
                intake_stories = CASE WHEN intake_day IS ?2 THEN intake_stories ELSE 0 END + ?3,
                intake_bytes = CASE WHEN intake_day IS ?2 THEN intake_bytes ELSE 0 END + ?4,
                intake_day = ?2
            WHERE id = ?1
            "#,
            params![feed_id, day, stories as i64, bytes as i64],
        )?;
        Ok(())
    }

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
        let conn = self.lock();

        let pub_str = published_at.map(|dt| dt.to_rfc3339());
        let upd_str = updated_at.map(|dt| dt.to_rfc3339());
        let fetched_now = Utc::now().to_rfc3339();

        // Check if article already exists
        // Whether the text differs is worked out by SQLite, so stored bodies
        // aren't copied out just to be compared
        let existing: Option<(i64, bool, Option<String>)> = conn.query_row(
            r#"
            SELECT id,
                   NOT (title IS ?3 AND summary IS ?4 AND (?5 IS NULL OR content_html IS ?5)),
                   updated_at
            FROM articles WHERE feed_id = ?1 AND guid_hash = ?2
            "#,
            params![feed_id, guid_hash, title, summary, content_html],
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
            Some((article_id, text_changed, old_updated)) => {
                // Only a change in the text counts. Some feeds stamp every
                // story with the time the feed was generated, so a new
                // "updated" date alone means nothing; it's stored without
                // rewriting the story or its search index.
                if !text_changed {
                    if upd_str.is_some() && upd_str != old_updated {
                        conn.execute(
                            "UPDATE articles SET updated_at = ?1 WHERE id = ?2",
                            params![upd_str, article_id],
                        )?;
                    }
                    return Ok(IngestionResult::Unchanged);
                }
                {
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
                }
            }
        }
    }

    /// The WHERE clause (without "WHERE") and its parameters for `filter`,
    /// shared by `get_articles` and `count_articles`.
    fn filter_clause(filter: &ArticleFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clause = String::from("1=1");
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(feed_id) = filter.feed_id {
            clause.push_str(" AND a.feed_id = ?");
            params_vec.push(Box::new(feed_id));
        }

        if let Some(folder_id) = filter.folder_id {
            clause.push_str(" AND a.feed_id IN (SELECT id FROM feeds WHERE folder_id = ?)");
            params_vec.push(Box::new(folder_id));
        }

        if let Some(article_id) = filter.article_id {
            clause.push_str(" AND a.id = ?");
            params_vec.push(Box::new(article_id));
        }

        // Searching All Stories finds tagged stories too (so none is ever
        // out of reach); searching a folder, feed or Unread keeps to what
        // they show
        let searching_everything = filter.search_query.as_ref().is_some_and(|q| !q.trim().is_empty())
            && filter.folder_id.is_none()
            && filter.feed_id.is_none()
            && !filter.unread_only;
        if let Some(tag_id) = filter.tag_id {
            clause.push_str(" AND a.id IN (SELECT article_id FROM story_tags WHERE tag_id = ?)");
            params_vec.push(Box::new(tag_id));
        } else if !searching_everything && !filter.starred_only {
            clause.push_str(" AND NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)");
        }

        if filter.unread_only {
            clause.push_str(" AND a.is_read = 0");
        }

        if filter.starred_only {
            clause.push_str(" AND a.is_starred = 1");
        }

        if let Some(ref search) = filter.search_query {
            if !search.trim().is_empty() {
                match fts_query(search) {
                    Some(query) => {
                        clause.push_str(" AND a.id IN (SELECT rowid FROM articles_fts WHERE articles_fts MATCH ?)");
                        params_vec.push(Box::new(query));
                    }
                    // Only punctuation: no word to look for
                    None => clause.push_str(" AND 0"),
                }
            }
        }

        (clause, params_vec)
    }

    /// How many stories match `filter` (its limit and offset aside).
    pub fn count_articles(&self, filter: &ArticleFilter) -> Result<usize> {
        let conn = self.lock();
        let (clause, params_vec) = Self::filter_clause(filter);
        let query = format!("SELECT COUNT(*) FROM articles a WHERE {}", clause);
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let count: i64 = conn.query_row(&query, param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as usize)
    }

    /// How many stories matching `filter` come before story `article_id` in
    /// the list order (its index in the full list, if it's part of it).
    pub fn count_before(&self, filter: &ArticleFilter, article_id: i64) -> Result<usize> {
        let conn = self.lock();
        let (clause, clause_params) = Self::filter_clause(filter);
        // "Before" in the list order (see `get_articles`): newer, or as new
        // with a higher id; with unread first, any unread story comes before
        // a read one
        let newer = "(MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) > t.k \
                     OR (MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) = t.k AND a.id > ?))";
        let before = if filter.unread_first {
            format!("(a.is_read < t.r OR (a.is_read = t.r AND {}))", newer)
        } else {
            newer.to_string()
        };
        let query = format!(
            r#"
            SELECT COUNT(*)
            FROM articles a,
                 (SELECT MIN(COALESCE(published_at, fetched_at), fetched_at) AS k, is_read AS r
                  FROM articles WHERE id = ?) t
            WHERE {} AND {}
            "#,
            clause, before
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(article_id)];
        params_vec.extend(clause_params);
        params_vec.push(Box::new(article_id));
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let count: i64 = conn.query_row(&query, param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Stories matching `filter`, newest first, for the story list. Use
    /// `limit` to read just the part on screen: with an archive of hundreds
    /// of thousands of stories, reading them all takes seconds.
    ///
    /// Bodies (`summary`, `content_html`, `content_text`) are left out (`None`):
    /// they add up to tens of MB over a few thousand stories and the list never
    /// shows them. Use `get_article` for one complete story.
    pub fn get_articles(&self, filter: &ArticleFilter) -> Result<Vec<Article>> {
        let conn = self.lock();
        let (clause, params_vec) = Self::filter_clause(filter);

        // Newest first by publication date, but a date in the future counts
        // as the time the story was downloaded (see `Article::shown_date`).
        // Both are stored as UTC RFC 3339 text, which sorts chronologically.
        // The expression matches the `idx_articles_*shown_date` indexes.
        let mut query = format!(
            r#"
            SELECT
                a.id, a.feed_id, f.title as feed_title, a.guid, a.guid_hash,
                a.title, a.url, a.author, NULL, NULL, NULL,
                a.published_at, a.updated_at, a.fetched_at, a.is_read, a.is_starred,
                (SELECT tag_id FROM story_tags WHERE article_id = a.id)
            FROM articles a
            JOIN feeds f ON f.id = a.feed_id
            WHERE {}
            ORDER BY {}MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) DESC, a.id DESC
            "#,
            clause,
            if filter.unread_first { "a.is_read, " } else { "" }
        );

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
                tag_id: row.get(16)?,
            })
        })?;

        let mut articles = Vec::new();
        for article in rows {
            articles.push(article?);
        }
        Ok(articles)
    }

    pub fn get_article(&self, id: i64) -> Result<Option<Article>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT 
                a.id, a.feed_id, f.title as feed_title, a.guid, a.guid_hash,
                a.title, a.url, a.author, a.summary, a.content_html, a.content_text,
                a.published_at, a.updated_at, a.fetched_at, a.is_read, a.is_starred,
                (SELECT tag_id FROM story_tags WHERE article_id = a.id)
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
                tag_id: row.get(16)?,
            })
        }).optional()?;

        Ok(result)
    }

    pub fn set_article_read(&self, id: i64, is_read: bool) -> Result<()> {
        let conn = self.lock();
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
        self.set_articles_read_state(feed_id, folder_id, true)
    }

    /// Marks every story in a feed, else a folder, else everywhere, as read or
    /// unread. Stories sorted into a tag aren't shown there, so they're left
    /// as they are (see `set_tag_read`).
    pub fn set_articles_read_state(
        &self,
        feed_id: Option<i64>,
        folder_id: Option<i64>,
        is_read: bool,
    ) -> Result<()> {
        let conn = self.lock();
        let is_read = is_read as i64;
        const UNTAGGED: &str = "NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = articles.id)";
        if let Some(feed_id) = feed_id {
            conn.execute(
                &format!("UPDATE articles SET is_read = ?1 WHERE feed_id = ?2 AND is_read != ?1 AND {}", UNTAGGED),
                params![is_read, feed_id],
            )?;
        } else if let Some(folder_id) = folder_id {
            conn.execute(
                &format!(
                    "UPDATE articles SET is_read = ?1 WHERE feed_id IN (SELECT id FROM feeds WHERE folder_id = ?2) AND is_read != ?1 AND {}",
                    UNTAGGED
                ),
                params![is_read, folder_id],
            )?;
        } else {
            conn.execute(&format!("UPDATE articles SET is_read = ?1 WHERE is_read != ?1 AND {}", UNTAGGED), params![is_read])?;
        }
        Ok(())
    }

    /// Marks every story sorted into a tag as read.
    pub fn set_tag_read(&self, tag_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE articles SET is_read = 1 WHERE is_read = 0 AND id IN (SELECT article_id FROM story_tags WHERE tag_id = ?1)",
            [tag_id],
        )?;
        Ok(())
    }

    /// The tags in use (turned on for a folder), in order, with how many of
    /// their stories are unread and in all
    pub fn tag_views(&self) -> Result<Vec<TagView>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name,
                    (SELECT COUNT(*) FROM story_tags s JOIN articles a ON a.id = s.article_id WHERE s.tag_id = t.id AND a.is_read = 0),
                    (SELECT COUNT(*) FROM story_tags s WHERE s.tag_id = t.id)
             FROM tags t
             WHERE t.removed = 0 AND EXISTS (SELECT 1 FROM folder_tags ft WHERE ft.tag_id = t.id)
             ORDER BY t.sort_order, t.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TagView { id: r.get(0)?, name: r.get(1)?, unread_count: r.get(2)?, total_count: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Corrects a story's tag while reading: it is (`true`) or isn't in `tag`.
    /// Recorded as the user's answer, for the trainer to learn from, and the
    /// story is sorted again at once.
    pub fn correct_tag(&self, article_id: i64, tag_id: i64, is_tag: bool) -> Result<()> {
        let conn = self.lock();
        shreader_tags::store::set_label(&conn, article_id, tag_id, Some(is_tag), shreader_tags::store::Source::Reader)?;
        shreader_tags::store::sort_into_tags(&conn)?;
        Ok(())
    }

    pub fn toggle_article_starred(&self, id: i64) -> Result<bool> {
        let conn = self.lock();
        let new_val: bool = conn.query_row(
            "UPDATE articles SET is_starred = NOT is_starred WHERE id = ?1 RETURNING is_starred",
            params![id],
            |row| Ok(row.get::<_, i64>(0)? != 0),
        )?;
        Ok(new_val)
    }

    pub fn get_total_stats(&self) -> Result<(i64, i64, i64)> {
        let conn = self.lock();
        let (total_feeds, total_articles, unread_articles) = conn.query_row(
            r#"
            SELECT
                (SELECT COUNT(*) FROM feeds),
                (SELECT COUNT(*) FROM articles a WHERE NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = a.id)),
                (SELECT COUNT(*) FROM articles a WHERE is_read = 0 AND NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = a.id))
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok((total_feeds, total_articles, unread_articles))
    }
}

/// Setting: the one-time `decode_stored_titles` has run
const TITLES_DECODED_SETTING: &str = "titles_decoded";

/// Decodes HTML character references in stored story titles and authors
/// (as `feed::Markup::References`). Only rows that look like they hold one
/// are read, and only changed ones written. How many stories changed.
fn decode_stored_titles(conn: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, title, author FROM articles WHERE title GLOB '*&*;*' OR author GLOB '*&*;*'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut changed = 0;
    for (id, title, author) in rows {
        use crate::feed::{feed_line, Markup, MAX_AUTHOR_CHARS, MAX_TITLE_CHARS};
        // Which kind of feed it came from isn't stored: references only, the
        // way RSS titles and all authors are read now
        let new_title = feed_line(&title, Markup::References, MAX_TITLE_CHARS);
        let new_author = author
            .as_deref()
            .map(|a| feed_line(a, Markup::References, MAX_AUTHOR_CHARS));
        if new_title != title || new_author != author {
            conn.execute(
                "UPDATE articles SET title = ?1, author = ?2 WHERE id = ?3",
                params![new_title, new_author, id],
            )?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// What's typed in the search box, as an FTS5 query.
///
/// Each word is looked for on its own, in any order, and the last one also
/// matches as the start of a longer word while it's still being typed:
/// `solar pan` finds "solar panels". A space after it ends the word, and
/// text in quotes is an exact phrase. Everything is quoted for FTS5, so
/// what's typed is never read as its syntax (`OR`, `-`, `:`, `*`).
/// `None` if there's no word at all, only punctuation.
fn fts_query(input: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut rest = input;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // (the term, what follows it, whether it's a phrase with its closing quote)
        let (term, after, closed) = match rest.strip_prefix('"') {
            Some(phrase) => match phrase.find('"') {
                Some(end) => (&phrase[..end], &phrase[end + 1..], true),
                None => (phrase, "", false), // being typed
            },
            None => {
                let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
                (&rest[..end], &rest[end..], false)
            }
        };
        rest = after;
        // Punctuation alone isn't a word (and would make an empty phrase)
        if !term.chars().any(char::is_alphanumeric) {
            continue;
        }
        let mut quoted = format!("\"{}\"", term);
        if after.is_empty() && !closed && !input.ends_with(char::is_whitespace) {
            quoted.push('*');
        }
        terms.push(quoted);
    }
    (!terms.is_empty()).then(|| terms.join(" "))
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
    fn refresh_keeps_user_changes_and_deleted_feeds_gone() {
        let db = Database::open_in_memory().unwrap();
        let folder = db.create_folder("News").unwrap();
        // Still the URL placeholder: a refresh fills in the real title
        let placeholder = db
            .add_or_update_feed("https://a.com/feed", "https://a.com/feed", None, None, None)
            .unwrap();
        assert_eq!(
            db.update_feed_from_fetch(placeholder, Some("A Blog"), None, Some("https://a.com"))
                .unwrap()
                .as_deref(),
            Some("A Blog")
        );
        // Renamed and moved by the user: a refresh leaves both alone
        let renamed = db
            .add_or_update_feed("https://b.com/feed", "My name for B", None, None, Some(folder))
            .unwrap();
        db.update_feed_from_fetch(renamed, Some("B's own title"), Some("about B"), None)
            .unwrap();
        let b = db.get_feed(renamed).unwrap().unwrap();
        assert_eq!(b.title, "My name for B");
        assert_eq!(b.folder_id, Some(folder));
        // Deleted while downloading: not brought back
        db.delete_feed(renamed).unwrap();
        assert_eq!(db.update_feed_from_fetch(renamed, Some("B"), None, None).unwrap(), None);
        assert!(db.get_feeds().unwrap().iter().all(|f| f.url != "https://b.com/feed"));
    }

    #[test]
    fn only_text_changes_count_as_updates() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://a.com/feed", "A", None, None, None).unwrap();
        let date = Utc::now();
        let later = date + chrono::Duration::hours(1);
        let upsert = |title: &str, html: Option<&str>, updated| {
            db.upsert_article(feed, Some("g1"), "h1", title, None, None, Some("s"), html, Some("text"), None, Some(updated))
                .unwrap()
        };
        assert!(matches!(upsert("Title", Some("<p>a</p>"), date), IngestionResult::Inserted));
        assert!(matches!(upsert("Title", Some("<p>a</p>"), date), IngestionResult::Unchanged));
        // A new date alone (feeds that stamp the generation time): unchanged,
        // but the date is kept
        assert!(matches!(upsert("Title", Some("<p>a</p>"), later), IngestionResult::Unchanged));
        let stored: Option<String> = db.conn.lock().unwrap()
            .query_row("SELECT updated_at FROM articles", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, Some(later.to_rfc3339()));
        // Real edits
        assert!(matches!(upsert("New title", Some("<p>a</p>"), later), IngestionResult::Updated));
        assert!(matches!(upsert("New title", Some("<p>edited</p>"), later), IngestionResult::Updated));
        // A fetch without the full content doesn't erase or "change" it
        assert!(matches!(upsert("New title", None, later), IngestionResult::Unchanged));
    }

    #[test]
    fn read_and_star_changes_leave_search_index_alone() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://a.com/feed", "A", None, None, None).unwrap();
        db.upsert_article(feed, Some("g1"), "h1", "Needle story", None, None, None, None, Some("body"), None, None)
            .unwrap();
        let id = db.get_articles(&ArticleFilter::default()).unwrap()[0].id;
        let index_changes = |db: &Database, f: &dyn Fn()| {
            let conn = db.conn.lock().unwrap();
            let before = conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0)).unwrap();
            drop(conn);
            f();
            let conn = db.conn.lock().unwrap();
            conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0)).unwrap() - before
        };
        // One row changed, and nothing else (no index delete + insert)
        assert_eq!(index_changes(&db, &|| db.set_article_read(id, true).unwrap()), 1);
        assert_eq!(index_changes(&db, &|| { db.toggle_article_starred(id).unwrap(); }), 1);
        // A title change is still re-indexed
        db.upsert_article(feed, Some("g1"), "h1", "Haystack story", None, None, None, None, Some("body"), None, None)
            .unwrap();
        let search = |q: &str| {
            db.get_articles(&ArticleFilter { search_query: Some(q.into()), ..Default::default() })
                .unwrap()
                .len()
        };
        assert_eq!(search("haystack"), 1);
        assert_eq!(search("needle"), 0);
    }

    /// Titles stored with HTML codes in them are decoded once, and found
    /// by search as they read.
    #[test]
    fn stored_titles_are_decoded_once() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://f.example/rss", "F", None, None, None).unwrap();
        let add = |hash: &str, title: &str| {
            db.upsert_article(feed, Some(hash), hash, title, None, None, None, None, Some("body"), None, None).unwrap()
        };
        add("a", "Google&#8217;s AI satellites");
        add("b", "AT&T earnings; R&D");
        let conn = db.lock();
        conn.execute("UPDATE articles SET author = 'Pat O&#8217;Brien' WHERE guid_hash = 'a'", []).unwrap();
        assert_eq!(decode_stored_titles(&conn).unwrap(), 1, "only the story with codes changes");
        assert_eq!(decode_stored_titles(&conn).unwrap(), 0);
        let row = |hash: &str| {
            conn.query_row("SELECT title, author FROM articles WHERE guid_hash = ?1", [hash], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap()
        };
        assert_eq!(row("a"), ("Google’s AI satellites".into(), Some("Pat O’Brien".into())));
        assert_eq!(row("b").0, "AT&T earnings; R&D");
        drop(conn);
        let found = db.get_articles(&ArticleFilter { search_query: Some("google".into()), ..Default::default() });
        assert_eq!(found.unwrap().len(), 1, "re-indexed");
        // Opening ran it already, and won't again
        assert_eq!(db.get_setting(TITLES_DECODED_SETTING).unwrap().as_deref(), Some("true"));
    }

    #[test]
    fn search_text_becomes_an_fts_query() {
        assert_eq!(fts_query("solar pan").as_deref(), Some(r#""solar" "pan"*"#));
        assert_eq!(fts_query("solar pan ").as_deref(), Some(r#""solar" "pan""#), "a space ends the word");
        assert_eq!(fts_query(r#""solar panels" cheap"#).as_deref(), Some(r#""solar panels" "cheap"*"#));
        assert_eq!(fts_query(r#"cheap "solar panels""#).as_deref(), Some(r#""cheap" "solar panels""#));
        assert_eq!(fts_query(r#"cheap "solar pan"#).as_deref(), Some(r#""cheap" "solar pan"*"#));
        assert_eq!(fts_query("rust OR go -c++").as_deref(), Some(r#""rust" "OR" "go" "-c++"*"#));
        assert_eq!(fts_query("  - ; ").as_deref(), None);
        assert_eq!(fts_query(r#"""#).as_deref(), None);
    }

    /// Partly typed words find stories, in any order; quotes keep a phrase.
    #[test]
    fn search_finds_words_as_they_are_typed() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://f.example/rss", "F", None, None, None).unwrap();
        for (hash, title) in [("a", "Cheap solar panels for your roof"), ("b", "Panels discuss solar policy")] {
            db.upsert_article(feed, Some(hash), hash, title, None, None, None, None, Some("body"), None, None).unwrap();
        }
        let search = |q: &str| {
            let mut titles: Vec<String> = db
                .get_articles(&ArticleFilter { search_query: Some(q.into()), ..Default::default() })
                .unwrap()
                .into_iter()
                .map(|a| a.title)
                .collect();
            titles.sort();
            titles
        };
        assert_eq!(search("sol").len(), 2);
        assert_eq!(search("solar pan").len(), 2, "both words, either order");
        assert_eq!(search("cheap sol"), ["Cheap solar panels for your roof"]);
        assert_eq!(search("\"solar pan"), ["Cheap solar panels for your roof"], "a phrase keeps its order");
        assert!(search("sol ").is_empty(), "\"sol\" on its own isn't a word in either");
        assert!(search("-").is_empty());
        assert!(search("roof OR").is_empty(), "OR is a word to find, not an operator");
    }

    /// A story dated in the future sorts (and shows) as of when it was
    /// downloaded, instead of staying above newer stories.
    #[test]
    fn future_dated_stories_sort_by_download_time() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://f.example/rss", "F", None, None, None).unwrap();
        let now = Utc::now();
        let add = |hash: &str, published: DateTime<Utc>| {
            db.upsert_article(feed, None, hash, hash, None, None, None, None, None, Some(published), None)
                .unwrap();
        };
        add("two hours ago", now - chrono::Duration::hours(2));
        add("tomorrow", now + chrono::Duration::days(1));
        add("next week", now + chrono::Duration::days(7));
        add("one hour ago", now - chrono::Duration::hours(1));

        let articles = db.get_articles(&ArticleFilter::default()).unwrap();
        let titles: Vec<&str> = articles.iter().map(|a| a.title.as_str()).collect();
        // Both future stories count as downloaded just now (in download order)
        assert_eq!(titles[2..], ["one hour ago", "two hours ago"]);
        assert!(titles[..2].contains(&"tomorrow") && titles[..2].contains(&"next week"));
        for a in &articles {
            assert!(a.shown_date().unwrap() <= Utc::now());
        }
    }

    /// Pages of the list join up into exactly the full list, even with many
    /// stories sharing a date; counts and positions agree with it.
    #[test]
    fn paged_list_matches_full_list() {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://f.example/rss", "F", None, None, None).unwrap();
        let base = Utc::now() - chrono::Duration::days(30);
        for i in 0..40 {
            // Groups of four share a date
            let published = base + chrono::Duration::hours(i / 4);
            let hash = format!("s{}", i);
            db.upsert_article(feed, None, &hash, &hash, None, None, None, None, None, Some(published), None)
                .unwrap();
        }
        let all = ArticleFilter::default();
        let full: Vec<i64> = db.get_articles(&all).unwrap().iter().map(|a| a.id).collect();
        assert_eq!(db.count_articles(&all).unwrap(), 40);

        for limit in [1, 7, 13, 40, 50] {
            let page: Vec<i64> = db
                .get_articles(&ArticleFilter { limit: Some(limit), ..Default::default() })
                .unwrap()
                .iter()
                .map(|a| a.id)
                .collect();
            assert_eq!(page, full[..limit.min(40)], "limit {}", limit);
        }
        for (index, id) in full.iter().enumerate() {
            assert_eq!(db.count_before(&all, *id).unwrap(), index);
        }

        // Unread first: pages, counts and positions follow that order too
        // Every third story read
        for id in full.iter().step_by(3) {
            db.set_article_read(*id, true).unwrap();
        }
        let unread_first = ArticleFilter { unread_first: true, ..Default::default() };
        let ordered: Vec<i64> = db.get_articles(&unread_first).unwrap().iter().map(|a| a.id).collect();
        let expected: Vec<i64> = full
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 3 != 0)
            .chain(full.iter().enumerate().filter(|(i, _)| i % 3 == 0))
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(ordered, expected);
        for limit in [1, 7, 26, 27, 40] {
            let page: Vec<i64> = db
                .get_articles(&ArticleFilter { limit: Some(limit), ..unread_first.clone() })
                .unwrap()
                .iter()
                .map(|a| a.id)
                .collect();
            assert_eq!(page, ordered[..limit], "limit {}", limit);
        }
        for (index, id) in ordered.iter().enumerate() {
            assert_eq!(db.count_before(&unread_first, *id).unwrap(), index);
        }

        // Counts and positions within a narrower view
        db.toggle_article_starred(full[10]).unwrap();
        db.toggle_article_starred(full[20]).unwrap();
        let starred = ArticleFilter { starred_only: true, ..Default::default() };
        assert_eq!(db.count_articles(&starred).unwrap(), 2);
        assert_eq!(db.count_before(&starred, full[20]).unwrap(), 1);
        let only = |id| ArticleFilter { starred_only: true, article_id: Some(id), ..Default::default() };
        assert_eq!(db.get_articles(&only(full[20])).unwrap().len(), 1);
        assert!(db.get_articles(&only(full[21])).unwrap().is_empty());
    }

    /// A panic while the connection is held doesn't break the database for
    /// the rest of the session.
    #[test]
    fn survives_a_panic_while_locked() {
        let db = Database::open_in_memory().unwrap();
        let other = db.clone();
        let _ = std::thread::spawn(move || {
            let _held = other.lock();
            panic!("simulated bug");
        })
        .join();
        assert!(db.conn.is_poisoned());
        assert!(db.get_folders().is_ok());
    }

    /// An archive in the pre-rename location moves to the new one with all
    /// its data, including changes still in the write-ahead log; one that's
    /// open elsewhere stays put and keeps being used.
    #[test]
    fn archive_moves_to_the_new_location() {
        let root = std::env::temp_dir().join(format!("shreader-move-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let legacy = root.join("rss_reader/rss_reader.db");
        let new = root.join("shreader/shreader.db");

        let old_db = Database::open(&legacy).unwrap();
        old_db.create_folder("Kept").unwrap();
        {
            // Leave the change in the write-ahead log, as a running app would
            let conn = old_db.lock();
            conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        }
        old_db.create_folder("Also kept").unwrap();

        // Still open: not moved, and the old archive is the one to use
        assert_eq!(adopt_legacy_archive(&legacy, &new), legacy);
        assert!(legacy.exists() && !new.exists());

        drop(old_db);
        assert_eq!(adopt_legacy_archive(&legacy, &new), new);
        assert!(!legacy.exists() && !root.join("rss_reader").exists());
        let moved = Database::open(&new).unwrap();
        let names: Vec<String> = moved.get_folders().unwrap().into_iter().map(|f| f.name).collect();
        assert_eq!(names.len(), 2, "{:?}", names);

        // Once moved, a new archive is never replaced by an old one
        Database::open(&legacy).unwrap();
        assert_eq!(adopt_legacy_archive(&legacy, &new), new);
        assert!(legacy.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_order_folders() {
        let db = Database::open_in_memory().unwrap();
        let b = db.create_folder("Bravo").unwrap();
        let a = db.create_folder("alpha").unwrap();
        let c = db.create_folder("Charlie").unwrap();
        let names = |db: &Database| db.get_folders().unwrap().into_iter().map(|f| f.id).collect::<Vec<_>>();
        // Unarranged: alphabetical, ignoring case
        assert_eq!(names(&db), vec![a, b, c]);
        db.order_folders(&[c, a, b]).unwrap();
        assert_eq!(names(&db), vec![c, a, b]);
        // A folder created later goes after the arranged ones
        let d = db.create_folder("Aardvark").unwrap();
        assert_eq!(names(&db), vec![c, a, b, d]);
    }

    #[test]
    fn test_clear_stories() {
        let db = Database::open_in_memory().unwrap();
        let folder_id = db.create_folder("News").unwrap();
        let feed_id = db
            .add_or_update_feed("https://example.com/feed", "Example", None, None, Some(folder_id))
            .unwrap();
        db.update_feed_http_meta(feed_id, Some("etag-1"), None, Utc::now()).unwrap();
        for (guid, title, text) in [("g1", "Plain Story", "needle one"), ("g2", "Starred Story", "needle two")] {
            db.upsert_article(
                feed_id, Some(guid), guid, title, None, None,
                None, None, Some(text), Some(Utc::now()), None,
            )
            .unwrap();
        }
        let starred = db
            .get_articles(&ArticleFilter::default())
            .unwrap()
            .into_iter()
            .find(|a| a.title == "Starred Story")
            .unwrap();
        db.toggle_article_starred(starred.id).unwrap();
        let search = ArticleFilter { search_query: Some("needle".into()), ..Default::default() };

        // Keeping starred: only the plain story goes, from the list and from search
        assert_eq!(db.clear_stories(true).unwrap(), 1);
        let left = db.get_articles(&ArticleFilter::default()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "Starred Story");
        assert_eq!(db.get_articles(&search).unwrap().len(), 1);

        // Feeds and folders stay; caching info is reset so a refresh refetches
        let feed = db.get_feed(feed_id).unwrap().unwrap();
        assert_eq!(feed.folder_id, Some(folder_id));
        assert!(feed.etag.is_none());

        // Everything
        assert_eq!(db.clear_stories(false).unwrap(), 1);
        assert!(db.get_articles(&ArticleFilter::default()).unwrap().is_empty());
        assert!(db.get_articles(&search).unwrap().is_empty());
        assert_eq!(db.get_feeds().unwrap().len(), 1);
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

    /// Stories sorted into a tag are left out of their feed and folder, and
    /// of All and Unread, but are found by searching All and among starred
    #[test]
    fn tagged_stories_are_kept_apart() {
        use shreader_tags::store;
        let db = Database::open_in_memory().unwrap();
        let folder = db.create_folder("Tech").unwrap();
        let feed = db.add_or_update_feed("https://t.example/rss", "T", None, None, Some(folder)).unwrap();
        for title in ["Coupon codes for laptops", "Chip news"] {
            db.upsert_article(feed, None, title, title, None, None, None, Some(title), None, None, None).unwrap();
        }
        let promo = db.get_articles(&ArticleFilter::default()).unwrap().iter().find(|a| a.title.starts_with("Coupon")).unwrap().id;
        let tag = db.with_conn(|c| {
            let tag = store::create_tag(c, "Promotions").unwrap();
            store::set_folder_tag(c, folder, tag, true).unwrap();
            store::set_label(c, promo, tag, Some(true), store::Source::Trainer).unwrap();
            store::sort_into_tags(c).unwrap();
            tag
        });
        let count = |filter: ArticleFilter| db.count_articles(&filter).unwrap();
        assert_eq!(count(ArticleFilter::default()), 1);
        assert_eq!(count(ArticleFilter { unread_only: true, ..Default::default() }), 1);
        assert_eq!(count(ArticleFilter { folder_id: Some(folder), ..Default::default() }), 1);
        assert_eq!(count(ArticleFilter { feed_id: Some(feed), ..Default::default() }), 1);
        assert_eq!(count(ArticleFilter { tag_id: Some(tag), ..Default::default() }), 1);
        assert_eq!(count(ArticleFilter { search_query: Some("coupon".into()), ..Default::default() }), 1);
        // Searching a folder or feed keeps to what they show
        let search = |filter: ArticleFilter| count(ArticleFilter { search_query: Some("coupon".into()), ..filter });
        assert_eq!(search(ArticleFilter { folder_id: Some(folder), ..Default::default() }), 0);
        assert_eq!(search(ArticleFilter { feed_id: Some(feed), ..Default::default() }), 0);
        assert_eq!(search(ArticleFilter { unread_only: true, ..Default::default() }), 0);
        assert_eq!(search(ArticleFilter { tag_id: Some(tag), ..Default::default() }), 1);
        db.toggle_article_starred(promo).unwrap();
        assert_eq!(count(ArticleFilter { starred_only: true, ..Default::default() }), 1);
        assert_eq!(db.get_article(promo).unwrap().unwrap().tag_id, Some(tag));
        assert_eq!(db.get_total_stats().unwrap(), (1, 1, 1));
        assert_eq!(db.get_folders().unwrap()[0].total_count, 1);
        assert_eq!(db.get_feeds().unwrap()[0].unread_count, 1);

        // Marking the folder read leaves the tag's story; the tag has its own
        db.set_all_articles_read(None, Some(folder)).unwrap();
        assert_eq!(db.tag_views().unwrap()[0].unread_count, 1);
        db.set_tag_read(tag).unwrap();
        assert_eq!(db.tag_views().unwrap()[0].unread_count, 0);

        // Corrected while reading: back in the folder
        db.correct_tag(promo, tag, false).unwrap();
        assert_eq!(count(ArticleFilter { folder_id: Some(folder), ..Default::default() }), 2);
        assert_eq!(db.tag_views().unwrap()[0].total_count, 0);
    }
}
