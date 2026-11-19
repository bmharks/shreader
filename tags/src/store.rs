//! Tags in the archive: which tags there are and which folders use them,
//! the user's answers, the trained models, story embeddings, and which tag
//! each story ended up in. Shared by Shreader and the trainer, which each
//! open the archive; these tables are the only ones the trainer changes.
//!
//! A story is in at most one tag. Tags are tried in their order (the oldest
//! first), and only those turned on for the story's folder: the first that
//! claims it gets it. The user's answers are final: a story they said is in
//! a tag is, and one they said isn't, isn't, whatever the model thinks.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::embedder;
use crate::model::{Evaluation, Example, TagModel};

/// Creates the tables if they aren't there yet. Safe to call every time
/// the archive is opened.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- Story embeddings (see embedder.rs), removed with their story. The
        -- same table as story grouping's, so either can use the other's.
        CREATE TABLE IF NOT EXISTS story_embeddings (
            article_id INTEGER PRIMARY KEY REFERENCES articles(id) ON DELETE CASCADE,
            vector BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            -- The order tags are tried in
            sort_order INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            -- Deleted (see `delete_tag`): hidden, its answers and model kept
            removed INTEGER NOT NULL DEFAULT 0
        );
        -- The tags each folder's stories are sorted into
        CREATE TABLE IF NOT EXISTS folder_tags (
            folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
            tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY (folder_id, tag_id)
        );
        -- The user's answers: whether a story is in a tag. From the trainer
        -- or from correcting a story in Shreader (`source`).
        CREATE TABLE IF NOT EXISTS tag_labels (
            article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
            tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            is_tag INTEGER NOT NULL,
            source TEXT NOT NULL,
            labelled_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (article_id, tag_id)
        );
        -- Each tag's model in use, and how it did when it was trained
        CREATE TABLE IF NOT EXISTS tag_models (
            tag_id INTEGER PRIMARY KEY REFERENCES tags(id) ON DELETE CASCADE,
            model BLOB NOT NULL,
            summary TEXT NOT NULL,
            trained_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        -- The tag each story is in (stories in none aren't listed)
        CREATE TABLE IF NOT EXISTS story_tags (
            article_id INTEGER PRIMARY KEY REFERENCES articles(id) ON DELETE CASCADE,
            tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_story_tags_tag ON story_tags(tag_id);
        CREATE INDEX IF NOT EXISTS idx_tag_labels_tag ON tag_labels(tag_id);
        ",
    )
    .context("creating the tag tables")?;
    // Tags tables made before deleting was possible
    let _ = conn.execute("ALTER TABLE tags ADD COLUMN removed INTEGER NOT NULL DEFAULT 0", []);
    Ok(())
}

/// Where an answer came from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Reviewing stories in the trainer
    Trainer,
    /// Correcting a story's tag while reading, in Shreader
    Reader,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Trainer => "trainer",
            Source::Reader => "reader",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
}

/// Every tag (deleted ones aside), in the order they're tried
pub fn tags(conn: &Connection) -> Result<Vec<Tag>> {
    let mut stmt = conn.prepare("SELECT id, name FROM tags WHERE removed = 0 ORDER BY sort_order, id")?;
    let rows = stmt.query_map([], |r| Ok(Tag { id: r.get(0)?, name: r.get(1)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Adds a tag, tried after the existing ones. Its id. A deleted tag of the
/// same name comes back instead, with its answers and model (but no
/// folders: those are chosen again).
pub fn create_tag(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a tag needs a name");
    let deleted: Option<i64> = conn
        .query_row("SELECT id FROM tags WHERE name = ?1 AND removed = 1", [name], |r| r.get(0))
        .optional()?;
    if let Some(id) = deleted {
        conn.execute(
            "UPDATE tags SET removed = 0, name = ?2, sort_order = (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM tags)
             WHERE id = ?1",
            params![id, name],
        )?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO tags (name, sort_order) VALUES (?1, (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM tags))",
        [name],
    )
    .with_context(|| format!("there's already a tag called {:?}", name))?;
    Ok(conn.last_insert_rowid())
}

/// Deletes a tag: its stories go back to their folders, and it's no longer
/// used or listed. The user's answers about it and its model are kept, in
/// case a tag of the same name is made again (see `create_tag`).
pub fn delete_tag(conn: &Connection, tag: i64) -> Result<()> {
    conn.execute("UPDATE tags SET removed = 1 WHERE id = ?1", [tag])?;
    conn.execute("DELETE FROM folder_tags WHERE tag_id = ?1", [tag])?;
    sort_into_tags(conn)?;
    Ok(())
}

/// (folder, tag) for every tag turned on for a folder
pub fn folder_tags(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT folder_id, tag_id FROM folder_tags")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Turns a tag on or off for a folder.
pub fn set_folder_tag(conn: &Connection, folder: i64, tag: i64, on: bool) -> Result<()> {
    if on {
        conn.execute("INSERT OR IGNORE INTO folder_tags (folder_id, tag_id) VALUES (?1, ?2)", [folder, tag])?;
    } else {
        conn.execute("DELETE FROM folder_tags WHERE folder_id = ?1 AND tag_id = ?2", [folder, tag])?;
    }
    Ok(())
}

/// Records the user's answer about a story (`None` forgets it). Saying a
/// story is in one tag also says it's in none of the others.
pub fn set_label(conn: &Connection, article: i64, tag: i64, is_tag: Option<bool>, source: Source) -> Result<()> {
    match is_tag {
        None => {
            conn.execute("DELETE FROM tag_labels WHERE article_id = ?1 AND tag_id = ?2", [article, tag])?;
        }
        Some(is_tag) => {
            if is_tag {
                conn.execute(
                    "UPDATE tag_labels SET is_tag = 0, source = ?3, labelled_at = datetime('now')
                     WHERE article_id = ?1 AND tag_id != ?2 AND is_tag = 1",
                    params![article, tag, source.as_str()],
                )?;
            }
            conn.execute(
                "INSERT OR REPLACE INTO tag_labels (article_id, tag_id, is_tag, source) VALUES (?1, ?2, ?3, ?4)",
                params![article, tag, is_tag, source.as_str()],
            )?;
        }
    }
    Ok(())
}

/// The user's answer about a story and a tag, if any
pub fn label(conn: &Connection, article: i64, tag: i64) -> Result<Option<bool>> {
    Ok(conn
        .query_row("SELECT is_tag FROM tag_labels WHERE article_id = ?1 AND tag_id = ?2", [article, tag], |r| r.get(0))
        .optional()?)
}

/// What a tag's model learns from: the user's answers about it, and stories
/// they put in another tag (which aren't in this one). Stories without an
/// embedding yet are left out (see `stories_to_embed`).
pub fn training_examples(conn: &Connection, tag: i64) -> Result<Vec<Example>> {
    let mut stmt = conn.prepare(
        "SELECT a.feed_id, e.vector, MAX(l.tag_id = ?1 AND l.is_tag = 1)
         FROM tag_labels l
         JOIN articles a ON a.id = l.article_id
         JOIN story_embeddings e ON e.article_id = l.article_id
         WHERE l.tag_id = ?1 OR l.is_tag = 1
         GROUP BY l.article_id
         ORDER BY l.article_id",
    )?;
    let rows = stmt.query_map([tag], |r| {
        Ok(Example { feed: r.get(0)?, embedding: embedder::from_bytes(&r.get::<_, Vec<u8>>(1)?), is_tag: r.get(2)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Makes `model` the one used for `tag`.
pub fn save_model(conn: &Connection, tag: i64, model: &TagModel, evaluation: &Evaluation) -> Result<()> {
    let summary = format!("{} model: {}", evaluation.kind.name(), evaluation.describe());
    conn.execute(
        "INSERT OR REPLACE INTO tag_models (tag_id, model, summary, trained_at) VALUES (?1, ?2, ?3, datetime('now'))",
        params![tag, model.to_bytes(), summary],
    )?;
    Ok(())
}

/// A tag's model in use, with what it says about itself and when it was
/// trained (UTC, "YYYY-MM-DD HH:MM:SS")
pub fn model(conn: &Connection, tag: i64) -> Result<Option<(TagModel, String, String)>> {
    let row: Option<(Vec<u8>, String, String)> = conn
        .query_row("SELECT model, summary, trained_at FROM tag_models WHERE tag_id = ?1", [tag], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    row.map(|(bytes, summary, at)| Ok((TagModel::from_bytes(&bytes)?, summary, at))).transpose()
}

/// Stories in folders with tags only get embeddings (and so a model's
/// opinion) from this many days back; older ones keep whatever tag the
/// user gave them.
pub const RECENT_DAYS: i64 = 30;

/// Up to `limit` stories in folders with a tag, dated `since` or later
/// (newest first), with no embedding yet: (id, text to embed).
pub fn stories_to_embed(conn: &Connection, since: &str, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.title, substr(COALESCE(a.content_text, ''), 1, 600)
         FROM articles a LEFT JOIN story_embeddings e ON e.article_id = a.id
         WHERE e.article_id IS NULL
           AND a.feed_id IN (SELECT fe.id FROM feeds fe JOIN folder_tags ft ON ft.folder_id = fe.folder_id)
           AND MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) >= ?1
         ORDER BY MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at) DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit as i64], |r| {
        let (title, text): (String, String) = (r.get(1)?, r.get(2)?);
        Ok((r.get(0)?, embedder::story_text(&title, &text)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Stores embeddings.
pub fn set_embeddings(conn: &Connection, rows: &[(i64, Vec<f32>)]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO story_embeddings (article_id, vector) VALUES (?1, ?2)")?;
        for (id, vector) in rows {
            stmt.execute(params![id, embedder::to_bytes(vector)])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The embeddings of those of `ids` that have one
pub fn embeddings(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, Vec<f32>>> {
    let mut stmt = conn.prepare("SELECT vector FROM story_embeddings WHERE article_id = ?1")?;
    let mut found = HashMap::new();
    for &id in ids {
        if let Some(bytes) = stmt.query_row([id], |r| r.get::<_, Vec<u8>>(0)).optional()? {
            found.insert(id, embedder::from_bytes(&bytes));
        }
    }
    Ok(found)
}

/// Sorts every story in a folder with tags into its tag (or none), from the
/// user's answers and the models. Returns how many stories are in a tag.
/// Quick (a few milliseconds per thousand stories), so it's simply redone
/// in full whenever stories, answers, models or folders' tags change.
pub fn sort_into_tags(conn: &Connection) -> Result<usize> {
    let order: Vec<i64> = tags(conn)?.into_iter().map(|t| t.id).collect();
    let mut models = HashMap::new();
    for &tag in &order {
        if let Some((model, _, _)) = model(conn, tag)? {
            models.insert(tag, model);
        }
    }
    let mut folder_tags_on: HashMap<i64, HashSet<i64>> = HashMap::new();
    for (folder, tag) in folder_tags(conn)? {
        folder_tags_on.entry(folder).or_default().insert(tag);
    }
    let mut labels: HashMap<(i64, i64), bool> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT l.article_id, l.tag_id, l.is_tag FROM tag_labels l
             JOIN articles a ON a.id = l.article_id
             WHERE a.feed_id IN (SELECT fe.id FROM feeds fe JOIN folder_tags ft ON ft.folder_id = fe.folder_id)",
        )?;
        let rows = stmt.query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?;
        for row in rows {
            let (key, value) = row?;
            labels.insert(key, value);
        }
    }

    let mut sorted: Vec<(i64, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT a.id, fe.folder_id, e.vector
             FROM articles a
             JOIN feeds fe ON fe.id = a.feed_id
             LEFT JOIN story_embeddings e ON e.article_id = a.id
             WHERE fe.folder_id IN (SELECT folder_id FROM folder_tags)",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let (id, folder): (i64, i64) = (row.get(0)?, row.get(1)?);
            let embedding: Option<Vec<f32>> = row.get::<_, Option<Vec<u8>>>(2)?.map(|b| embedder::from_bytes(&b));
            let on = &folder_tags_on[&folder];
            let tag = order.iter().copied().filter(|t| on.contains(t)).find(|t| match labels.get(&(id, *t)) {
                Some(&answer) => answer,
                None => embedding.as_ref().is_some_and(|e| models.get(t).is_some_and(|m| m.tags(e))),
            });
            if let Some(tag) = tag {
                sorted.push((id, tag));
            }
        }
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM story_tags", [])?;
    {
        let mut stmt = tx.prepare("INSERT INTO story_tags (article_id, tag_id) VALUES (?1, ?2)")?;
        for (id, tag) in &sorted {
            stmt.execute([id, tag])?;
        }
    }
    tx.commit()?;
    Ok(sorted.len())
}

/// Embeds stories that need it (see `stories_to_embed`), newest first, a
/// batch at a time. `with_conn` gives the archive: it's only held while
/// reading or writing, never while the (slow) embedding runs. Returns how
/// many it embedded. CPU-heavy: off the UI thread.
pub fn embed_new_stories<W>(with_conn: W, model_dir: &std::path::Path) -> Result<usize>
where
    W: Fn(&mut dyn FnMut(&Connection) -> Result<()>) -> Result<()>,
{
    const BATCH: usize = 32;
    let since = (chrono::Utc::now() - chrono::Duration::days(RECENT_DAYS)).to_rfc3339();
    // Loaded only once there's something to embed
    let mut guard = None;
    let mut embedded = 0;
    loop {
        let mut stories = Vec::new();
        with_conn(&mut |conn| {
            stories = stories_to_embed(conn, &since, BATCH)?;
            Ok(())
        })?;
        if stories.is_empty() {
            return Ok(embedded);
        }
        let texts: Vec<String> = stories.iter().map(|(_, text)| text.clone()).collect();
        if guard.is_none() {
            guard = Some(embedder::shared(model_dir)?);
        }
        let vectors = guard.as_ref().and_then(|g| g.as_ref()).expect("just loaded").embed(&texts)?;
        let rows: Vec<(i64, Vec<f32>)> = stories.iter().map(|(id, _)| *id).zip(vectors).collect();
        with_conn(&mut |conn| set_embeddings(conn, &rows))?;
        embedded += rows.len();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An archive with just what tags need from Shreader's tables (the
    /// columns used here), and `ensure_schema`
    pub fn archive() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE folders (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE feeds (id INTEGER PRIMARY KEY, folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL);
             CREATE TABLE articles (id INTEGER PRIMARY KEY, feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                 title TEXT NOT NULL, content_text TEXT, published_at TEXT, fetched_at TEXT NOT NULL DEFAULT (datetime('now')));
             INSERT INTO folders VALUES (1, 'Tech'), (2, 'World');
             INSERT INTO feeds VALUES (10, 1), (20, 2);",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    fn add_story(conn: &Connection, id: i64, feed: i64, title: &str) {
        conn.execute(
            "INSERT INTO articles (id, feed_id, title, published_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, feed, title, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    fn tag_of(conn: &Connection, id: i64) -> Option<i64> {
        conn.query_row("SELECT tag_id FROM story_tags WHERE article_id = ?1", [id], |r| r.get(0)).optional().unwrap()
    }

    #[test]
    fn tags_keep_their_order_and_names() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        let security = create_tag(&conn, " Security ").unwrap();
        assert!(create_tag(&conn, "promotions").is_err(), "names are unique, whatever the case");
        assert!(create_tag(&conn, "  ").is_err());
        let names: Vec<String> = tags(&conn).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["Promotions", "Security"]);
        set_folder_tag(&conn, 1, promos, true).unwrap();
        set_folder_tag(&conn, 1, promos, true).unwrap();
        set_folder_tag(&conn, 2, security, true).unwrap();
        set_folder_tag(&conn, 2, security, false).unwrap();
        assert_eq!(folder_tags(&conn).unwrap(), [(1, promos)]);
    }

    #[test]
    fn a_story_is_in_one_tag_at_most() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        let security = create_tag(&conn, "Security").unwrap();
        add_story(&conn, 1, 10, "Story");
        set_label(&conn, 1, promos, Some(true), Source::Trainer).unwrap();
        set_label(&conn, 1, security, Some(true), Source::Reader).unwrap();
        assert_eq!(label(&conn, 1, promos).unwrap(), Some(false));
        assert_eq!(label(&conn, 1, security).unwrap(), Some(true));
        set_label(&conn, 1, security, None, Source::Reader).unwrap();
        assert_eq!(label(&conn, 1, security).unwrap(), None);
    }

    #[test]
    fn examples_include_other_tags_stories() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        let security = create_tag(&conn, "Security").unwrap();
        for id in 1..=4 {
            add_story(&conn, id, 10, "Story");
        }
        let e = |n: usize| -> Vec<f32> { (0..crate::embedder::DIMENSIONS).map(|d| (d == n) as u8 as f32).collect() };
        set_embeddings(&conn, &[(1, e(1)), (2, e(2)), (3, e(3))]).unwrap();
        set_label(&conn, 1, promos, Some(true), Source::Trainer).unwrap();
        set_label(&conn, 2, promos, Some(false), Source::Trainer).unwrap();
        set_label(&conn, 3, security, Some(true), Source::Trainer).unwrap();
        // No embedding: left out
        set_label(&conn, 4, promos, Some(true), Source::Trainer).unwrap();
        let examples = training_examples(&conn, promos).unwrap();
        let answers: Vec<bool> = examples.iter().map(|e| e.is_tag).collect();
        assert_eq!(answers, [true, false, false]);
        assert_eq!(examples[2].embedding, e(3));
        assert_eq!(training_examples(&conn, security).unwrap().iter().filter(|e| e.is_tag).count(), 1);
    }

    #[test]
    fn stories_are_sorted_into_the_first_tag_that_claims_them() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        let security = create_tag(&conn, "Security").unwrap();
        for id in 1..=4 {
            add_story(&conn, id, 10, "Tech story");
        }
        add_story(&conn, 5, 20, "World story");
        // Both tags on for Tech; none for World
        set_folder_tag(&conn, 1, security, true).unwrap();
        set_folder_tag(&conn, 1, promos, true).unwrap();
        set_label(&conn, 1, promos, Some(true), Source::Trainer).unwrap();
        set_label(&conn, 2, security, Some(true), Source::Trainer).unwrap();
        set_label(&conn, 3, promos, Some(false), Source::Reader).unwrap();
        set_label(&conn, 5, promos, Some(true), Source::Trainer).unwrap();
        assert_eq!(sort_into_tags(&conn).unwrap(), 2);
        assert_eq!(tag_of(&conn, 1), Some(promos));
        assert_eq!(tag_of(&conn, 2), Some(security));
        assert_eq!(tag_of(&conn, 3), None);
        assert_eq!(tag_of(&conn, 5), None, "World doesn't use tags");
        // Turning a tag off for the folder takes its stories out of it
        set_folder_tag(&conn, 1, promos, false).unwrap();
        assert_eq!(sort_into_tags(&conn).unwrap(), 1);
        assert_eq!(tag_of(&conn, 1), None);
    }

    #[test]
    fn models_sort_stories_the_user_hasnt_answered() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        set_folder_tag(&conn, 1, promos, true).unwrap();
        // Promotions point one way, other stories another
        let near = |axis: usize, n: usize| -> Vec<f32> {
            let mut v: Vec<f32> = (0..crate::embedder::DIMENSIONS).map(|d| ((d * 7 + n * 13) % 11) as f32 * 0.004).collect();
            v[axis] += 1.0;
            let length = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / length).collect()
        };
        let mut rows = Vec::new();
        for id in 1..=30 {
            add_story(&conn, id, 10, "Story");
            let is_promo = id % 3 == 0;
            rows.push((id, near(if is_promo { 0 } else { 1 + (id as usize % 5) }, id as usize)));
            // The first 24 are answered; the model decides the rest
            if id <= 24 {
                set_label(&conn, id, promos, Some(is_promo), Source::Trainer).unwrap();
            }
        }
        set_embeddings(&conn, &rows).unwrap();
        let trained = TagModel::train(&training_examples(&conn, promos).unwrap()).unwrap();
        save_model(&conn, promos, &trained.model, &trained.evaluations[0]).unwrap();
        let (_, summary, _) = model(&conn, promos).unwrap().unwrap();
        assert!(summary.contains("catches 8 of 8"), "{}", summary);
        sort_into_tags(&conn).unwrap();
        for id in 25..=30 {
            assert_eq!(tag_of(&conn, id).is_some(), id % 3 == 0, "story {}", id);
        }
    }

    #[test]
    fn only_tagged_folders_recent_stories_are_embedded() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        add_story(&conn, 1, 10, "Tech");
        add_story(&conn, 2, 20, "World");
        conn.execute("INSERT INTO articles (id, feed_id, title, published_at, fetched_at) VALUES (3, 10, 'Old', '2020-01-01T00:00:00+00:00', '2020-01-01 00:00:00')", []).unwrap();
        let since = (chrono::Utc::now() - chrono::Duration::days(RECENT_DAYS)).to_rfc3339();
        assert!(stories_to_embed(&conn, &since, 10).unwrap().is_empty());
        set_folder_tag(&conn, 1, promos, true).unwrap();
        assert_eq!(stories_to_embed(&conn, &since, 10).unwrap(), [(1, "Tech".to_string())]);
    }

    #[test]
    fn a_deleted_tag_keeps_its_answers_and_model_for_next_time() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        let security = create_tag(&conn, "Security").unwrap();
        add_story(&conn, 1, 10, "Story");
        set_folder_tag(&conn, 1, promos, true).unwrap();
        set_label(&conn, 1, promos, Some(true), Source::Trainer).unwrap();
        sort_into_tags(&conn).unwrap();
        assert_eq!(tag_of(&conn, 1), Some(promos));
        delete_tag(&conn, promos).unwrap();
        assert_eq!(tag_of(&conn, 1), None, "its stories go back to their folders");
        assert_eq!(tags(&conn).unwrap(), [Tag { id: security, name: "Security".into() }]);
        assert!(folder_tags(&conn).unwrap().is_empty());
        // Made again (whatever the case): the same tag, answers and all, last in order
        assert_eq!(create_tag(&conn, "PROMOTIONS").unwrap(), promos);
        let names: Vec<String> = tags(&conn).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["Security", "PROMOTIONS"]);
        assert_eq!(label(&conn, 1, promos).unwrap(), Some(true));
        assert!(create_tag(&conn, "Promotions").is_err(), "it's back, so the name is taken");
    }

    #[test]
    fn a_deleted_story_takes_its_data_with_it() {
        let conn = archive();
        let promos = create_tag(&conn, "Promotions").unwrap();
        add_story(&conn, 1, 10, "Story");
        set_folder_tag(&conn, 1, promos, true).unwrap();
        set_label(&conn, 1, promos, Some(true), Source::Trainer).unwrap();
        set_embeddings(&conn, &[(1, vec![0.0; crate::embedder::DIMENSIONS])]).unwrap();
        sort_into_tags(&conn).unwrap();
        conn.execute("DELETE FROM articles WHERE id = 1", []).unwrap();
        for table in ["tag_labels", "story_tags", "story_embeddings"] {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{}", table);
        }
    }
}
