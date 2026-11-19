//! The trainer's access to Shreader's archive. It reads stories, feeds and
//! folders, and only changes the tag tables (see `shreader_tags::store`).
//! Shreader can be running at the same time: SQLite lets both use the file,
//! each waiting a moment if the other is writing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use shreader_tags::{embedder, store};

#[derive(Clone)]
pub struct Archive {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

/// A folder, and whether the tag being trained is turned on for it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub name: String,
}

/// A story as shown for review
#[derive(Debug, Clone)]
pub struct Story {
    pub id: i64,
    pub title: String,
    pub feed: String,
    pub date: Option<String>,
    pub url: Option<String>,
    pub text: String,
}

/// An answer, as it can be undone: the story's answers for every tag before it
#[derive(Debug, Clone)]
pub struct Answered {
    pub story: i64,
    previous: Vec<(i64, bool, String)>,
}

/// How many answers a tag has
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Stories said to be in the tag
    pub yes: usize,
    /// Stories said not to be (including those put in another tag)
    pub no: usize,
    /// Of those, corrections made while reading in Shreader
    pub from_reader: usize,
}

/// Longest story text shown, in characters
const TEXT_CHARS: usize = 1500;

impl Archive {
    /// Shreader's archive: `$SHREADER_DB`, or its standard place. It must
    /// already exist (the trainer never creates one).
    pub fn default_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("SHREADER_DB") {
            return Ok(PathBuf::from(path));
        }
        let dirs = directories::ProjectDirs::from("io.github", "bmharks", "Shreader")
            .context("couldn't work out where Shreader keeps its archive")?;
        Ok(dirs.data_dir().join("shreader.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("there's no Shreader archive at {}: run Shreader first", path.display());
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", path.display()))?;
        Self::from_connection(conn, path)
    }

    fn from_connection(conn: Connection, path: &Path) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let is_archive: bool = conn
            .query_row("SELECT COUNT(*) = 3 FROM sqlite_master WHERE type = 'table' AND name IN ('articles', 'feeds', 'folders')", [], |r| r.get(0))?;
        if !is_archive {
            bail!("{} isn't a Shreader archive", path.display());
        }
        store::ensure_schema(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), path: path.to_path_buf() })
    }

    #[cfg(test)]
    pub fn from_test_connection(conn: Connection) -> Self {
        Self::from_connection(conn, Path::new(":memory:")).unwrap()
    }

    /// The connection (a panic elsewhere while it was held leaves it usable)
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Where the embedding model is kept (next to the archive, as Shreader does)
    pub fn model_dir(&self) -> PathBuf {
        embedder::model_dir(&self.path)
    }

    /// Every folder, in Shreader's order
    pub fn folders(&self) -> Result<Vec<Folder>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, name FROM folders ORDER BY sort_order IS NULL, sort_order, name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| Ok(Folder { id: r.get(0)?, name: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stories that could be reviewed for `tag`, newest first, with their
    /// embeddings: in folders using the tag, embedded, not yet answered for
    /// it, and not put in another tag (by an answer or another tag's model).
    pub fn candidates(&self, tag: i64) -> Result<Vec<(i64, Vec<f32>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT a.id, e.vector FROM articles a
             JOIN feeds fe ON fe.id = a.feed_id
             JOIN folder_tags ft ON ft.folder_id = fe.folder_id AND ft.tag_id = ?1
             JOIN story_embeddings e ON e.article_id = a.id
             WHERE NOT EXISTS (SELECT 1 FROM tag_labels l WHERE l.article_id = a.id AND (l.tag_id = ?1 OR l.is_tag = 1))
               AND NOT EXISTS (SELECT 1 FROM story_tags s WHERE s.article_id = a.id AND s.tag_id != ?1)
             ORDER BY MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) DESC, a.id DESC",
        )?;
        let rows = stmt.query_map([tag], |r| Ok((r.get(0)?, embedder::from_bytes(&r.get::<_, Vec<u8>>(1)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Unanswered stories in folders using `tag` that match a search (any
    /// age), best matches first
    pub fn search(&self, tag: i64, query: &str) -> Result<Vec<i64>> {
        let words: Vec<String> =
            query.split_whitespace().map(|w| format!("\"{}\"", w.replace('"', ""))).filter(|w| w.len() > 2).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT a.id FROM articles_fts
             JOIN articles a ON a.id = articles_fts.rowid
             JOIN feeds fe ON fe.id = a.feed_id
             JOIN folder_tags ft ON ft.folder_id = fe.folder_id AND ft.tag_id = ?1
             WHERE articles_fts MATCH ?2
               AND NOT EXISTS (SELECT 1 FROM tag_labels l WHERE l.article_id = a.id AND (l.tag_id = ?1 OR l.is_tag = 1))
             ORDER BY rank
             LIMIT 500",
        )?;
        let rows = stmt.query_map(params![tag, words.join(" ")], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn story(&self, id: i64) -> Result<Option<Story>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT a.title, fe.title, MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at), a.url,
                        substr(COALESCE(NULLIF(a.content_text, ''), a.summary, ''), 1, ?2)
                 FROM articles a JOIN feeds fe ON fe.id = a.feed_id WHERE a.id = ?1",
                params![id, TEXT_CHARS as i64 * 2],
                |r| {
                    Ok(Story {
                        id,
                        title: r.get(0)?,
                        feed: r.get(1)?,
                        date: r.get(2)?,
                        url: r.get(3)?,
                        text: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row.map(|mut story| {
            story.text = tidy_text(&story.text);
            story
        }))
    }

    /// The embeddings of the stories said to be in `tag` (to find others
    /// like them before there's a model)
    pub fn positives(&self, tag: i64) -> Result<Vec<Vec<f32>>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.vector FROM tag_labels l JOIN story_embeddings e ON e.article_id = l.article_id
             WHERE l.tag_id = ?1 AND l.is_tag = 1",
        )?;
        let rows = stmt.query_map([tag], |r| Ok(embedder::from_bytes(&r.get::<_, Vec<u8>>(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn counts(&self, tag: i64) -> Result<Counts> {
        let conn = self.lock();
        let (yes, no, from_reader): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(yes), 0), COALESCE(SUM(1 - yes), 0), COALESCE(SUM(reader), 0) FROM (
                 SELECT MAX(tag_id = ?1 AND is_tag = 1) AS yes, MAX(tag_id = ?1 AND source = 'reader') AS reader
                 FROM tag_labels WHERE tag_id = ?1 OR is_tag = 1 GROUP BY article_id)",
            [tag],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(Counts { yes: yes as usize, no: no as usize, from_reader: from_reader as usize })
    }

    /// The latest answers about `tag`, newest first: (story, title, in the tag)
    pub fn recent_answers(&self, tag: i64, limit: usize) -> Result<Vec<(i64, String, bool)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT l.article_id, a.title, l.is_tag FROM tag_labels l JOIN articles a ON a.id = l.article_id
             WHERE l.tag_id = ?1 ORDER BY l.labelled_at DESC, l.rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![tag, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records an answer from the trainer. The story is embedded first if it
    /// isn't yet (a search can find older stories), so it can be learned from.
    pub fn answer(&self, story: i64, tag: i64, is_tag: bool) -> Result<Answered> {
        let missing: Option<String> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT a.title, substr(COALESCE(a.content_text, ''), 1, 600) FROM articles a
                 WHERE a.id = ?1 AND NOT EXISTS (SELECT 1 FROM story_embeddings e WHERE e.article_id = a.id)",
                [story],
                |r| Ok(embedder::story_text(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?)),
            )
            .optional()?
        };
        if let Some(text) = missing {
            let guard = embedder::shared(&self.model_dir())?;
            let vector = guard.as_ref().expect("loaded").embed(&[text])?.remove(0);
            store::set_embeddings(&self.lock(), &[(story, vector)])?;
        }
        let conn = self.lock();
        let previous = labels_of(&conn, story)?;
        store::set_label(&conn, story, tag, Some(is_tag), store::Source::Trainer)?;
        Ok(Answered { story, previous })
    }

    /// Puts a story's answers back as they were before `answered`.
    pub fn undo(&self, answered: &Answered) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM tag_labels WHERE article_id = ?1", [answered.story])?;
        for (tag, is_tag, source) in &answered.previous {
            tx.execute(
                "INSERT INTO tag_labels (article_id, tag_id, is_tag, source) VALUES (?1, ?2, ?3, ?4)",
                params![answered.story, tag, is_tag, source],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Embeds stories in folders with tags that need it (see
    /// `store::embed_new_stories`). Slow: off the UI thread.
    pub fn embed_new_stories(&self) -> Result<usize> {
        let dir = self.model_dir();
        store::embed_new_stories(|f| f(&self.lock()), &dir)
    }
}

/// A story's answers for every tag: (tag, in it, source)
fn labels_of(conn: &Connection, story: i64) -> Result<Vec<(i64, bool, String)>> {
    let mut stmt = conn.prepare("SELECT tag_id, is_tag, source FROM tag_labels WHERE article_id = ?1")?;
    let rows = stmt.query_map([story], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Story text for reading: paragraphs (split at blank lines) with their
/// lines and spaces run together, cut to `TEXT_CHARS` at a word
fn tidy_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut out = String::new();
    for paragraph in text.split("\n\n").map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&paragraph);
        if out.chars().count() > TEXT_CHARS {
            break;
        }
    }
    if out.chars().count() > TEXT_CHARS {
        let cut: String = out.chars().take(TEXT_CHARS).collect();
        let cut = cut.rsplit_once(' ').map(|(head, _)| head).unwrap_or(&cut);
        return format!("{}…", cut);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use shreader_tags::embedder::DIMENSIONS;

    /// An archive with Shreader's tables as far as the trainer uses them:
    /// folder 1 (Tech, feed 10), folder 2 (World, feed 20), stories 1-6 in
    /// Tech (1 newest) and 7 in World; all but 6 embedded
    fn archive() -> (Archive, i64, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE folders (id INTEGER PRIMARY KEY, name TEXT, sort_order INTEGER);
             CREATE TABLE feeds (id INTEGER PRIMARY KEY, folder_id INTEGER, title TEXT);
             CREATE TABLE articles (id INTEGER PRIMARY KEY, feed_id INTEGER NOT NULL REFERENCES feeds(id),
                 title TEXT NOT NULL, url TEXT, summary TEXT, content_text TEXT, published_at TEXT,
                 fetched_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE VIRTUAL TABLE articles_fts USING fts5(title, content='articles', content_rowid='id');
             INSERT INTO folders VALUES (1, 'Tech', 2), (2, 'World', 1);
             INSERT INTO feeds VALUES (10, 1, 'Tech feed'), (20, 2, 'World feed');",
        )
        .unwrap();
        let titles = ["Save 50% on laptops today", "Chip maker reports profits", "Promo codes for November",
            "Data breach at retailer", "New phone launched", "Old story", "World news"];
        for (n, title) in titles.iter().enumerate() {
            let id = n as i64 + 1;
            let feed = if id == 7 { 20 } else { 10 };
            conn.execute(
                "INSERT INTO articles (id, feed_id, title, content_text, published_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, feed, title, "Some text.\n\n\n  More   text.", format!("2026-11-{:02}T00:00:00+00:00", 19 - n)],
            )
            .unwrap();
            conn.execute("INSERT INTO articles_fts (rowid, title) VALUES (?1, ?2)", params![id, title]).unwrap();
        }
        let archive = Archive::from_test_connection(conn);
        let (promos, security) = {
            let conn = archive.lock();
            let rows: Vec<(i64, Vec<f32>)> =
                (1..=7).filter(|&id| id != 6).map(|id| (id, (0..DIMENSIONS).map(|d| (d as i64 == id) as u8 as f32).collect())).collect();
            store::set_embeddings(&conn, &rows).unwrap();
            let promos = store::create_tag(&conn, "Promotions").unwrap();
            let security = store::create_tag(&conn, "Security").unwrap();
            store::set_folder_tag(&conn, 1, promos, true).unwrap();
            store::set_folder_tag(&conn, 1, security, true).unwrap();
            (promos, security)
        };
        (archive, promos, security)
    }

    fn ids(candidates: Vec<(i64, Vec<f32>)>) -> Vec<i64> {
        candidates.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn folders_come_in_shreaders_order() {
        let (archive, _, _) = archive();
        let names: Vec<String> = archive.folders().unwrap().into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["World", "Tech"]);
    }

    #[test]
    fn candidates_are_unanswered_stories_in_the_tags_folders() {
        let (archive, promos, security) = archive();
        assert_eq!(ids(archive.candidates(promos).unwrap()), [1, 2, 3, 4, 5]);
        // Answered for this tag, or put in another: no longer a candidate
        archive.answer(1, promos, false).unwrap();
        archive.answer(4, security, true).unwrap();
        assert_eq!(ids(archive.candidates(promos).unwrap()), [2, 3, 5]);
        // Answered "no" for another tag: still one
        archive.answer(2, security, false).unwrap();
        assert_eq!(ids(archive.candidates(promos).unwrap()), [2, 3, 5]);
        // Sorted into another tag by its model
        archive.lock().execute("INSERT INTO story_tags VALUES (3, ?1)", [security]).unwrap();
        assert_eq!(ids(archive.candidates(promos).unwrap()), [2, 5]);
    }

    #[test]
    fn searching_finds_unanswered_stories_in_the_tags_folders() {
        let (archive, promos, _) = archive();
        assert_eq!(archive.search(promos, "promo codes").unwrap(), [3]);
        assert_eq!(archive.search(promos, "story").unwrap(), [6], "older and unembedded stories too");
        assert!(archive.search(promos, "world").unwrap().is_empty(), "World doesn't use the tag");
        assert!(archive.search(promos, "a \"").unwrap().is_empty());
        archive.answer(3, promos, true).unwrap();
        assert!(archive.search(promos, "promo").unwrap().is_empty());
    }

    #[test]
    fn answers_count_and_undo() {
        let (archive, promos, security) = archive();
        let first = archive.answer(1, promos, true).unwrap();
        archive.answer(2, promos, false).unwrap();
        archive.answer(4, security, true).unwrap();
        assert_eq!(archive.counts(promos).unwrap(), Counts { yes: 1, no: 2, from_reader: 0 });
        assert_eq!(archive.positives(promos).unwrap().len(), 1);
        // Changing one's mind: 1 is security after all
        let changed = archive.answer(1, security, true).unwrap();
        assert_eq!(archive.counts(promos).unwrap(), Counts { yes: 0, no: 3, from_reader: 0 });
        archive.undo(&changed).unwrap();
        assert_eq!(archive.counts(promos).unwrap(), Counts { yes: 1, no: 2, from_reader: 0 });
        archive.undo(&first).unwrap();
        assert_eq!(archive.counts(promos).unwrap(), Counts { yes: 0, no: 2, from_reader: 0 });
        store::set_label(&archive.lock(), 5, promos, Some(false), store::Source::Reader).unwrap();
        assert_eq!(archive.counts(promos).unwrap().from_reader, 1);
        let recent: Vec<i64> = archive.recent_answers(promos, 10).unwrap().into_iter().map(|r| r.0).collect();
        // 1's answer about Promotions was undone
        assert_eq!(recent.len(), 2);
        assert!(recent.contains(&5) && recent.contains(&2));
    }

    #[test]
    fn stories_read_tidily() {
        let (archive, _, _) = archive();
        let story = archive.story(2).unwrap().unwrap();
        assert_eq!((story.title.as_str(), story.feed.as_str()), ("Chip maker reports profits", "Tech feed"));
        assert_eq!(story.text, "Some text.\n\nMore text.");
        assert_eq!(tidy_text("One line\nwrapped.\r\n\r\nTwo"), "One line wrapped.\n\nTwo");
        assert!(archive.story(99).unwrap().is_none());
        let long = tidy_text(&"word ".repeat(1000));
        assert!(long.ends_with("word…") && long.chars().count() <= TEXT_CHARS + 1);
    }
}
