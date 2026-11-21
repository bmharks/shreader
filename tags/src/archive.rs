//! Where Shreader's archive is, and searching it: shared by Shreader and
//! the trainer, so both open the same file and search the same way.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::Connection;

/// The archive's schema version (`PRAGMA user_version`) this build uses:
/// how many of Shreader's schema steps it has had (see `SCHEMA_STEPS` in
/// Shreader's db.rs, which creates and upgrades the archive)
pub const SCHEMA_VERSION: i64 = 2;

/// An archive's schema version (0 for one from before versions began)
pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

/// Whether `table` has `column`
pub fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [table, column],
        |r| r.get(0),
    )?)
}

/// The archive chosen with `$SHREADER_DB` (or `$RSS_READER_DB`, from before
/// the rename), if any
pub fn path_from_env() -> Option<PathBuf> {
    std::env::var_os("SHREADER_DB").or_else(|| std::env::var_os("RSS_READER_DB")).map(PathBuf::from)
}

/// The archive's standard place, ~/.local/share/shreader/shreader.db (named
/// after the app ID, io.github.bmharks.Shreader)
pub fn standard_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("io.github", "bmharks", "Shreader")
        .context("Could not determine user XDG data directory")?;
    Ok(proj.data_dir().join("shreader.db"))
}

/// The archive to use: `$SHREADER_DB`, or the standard place
pub fn path() -> Result<PathBuf> {
    match path_from_env() {
        Some(path) => Ok(path),
        None => standard_path(),
    }
}

/// What's typed in the search box, as an FTS5 query.
///
/// Each word is looked for on its own, in any order, and the last one also
/// matches as the start of a longer word while it's still being typed:
/// `solar pan` finds "solar panels". A space after it ends the word, and
/// text in quotes is an exact phrase. Everything is quoted for FTS5, so
/// what's typed is never read as its syntax (`OR`, `-`, `:`, `*`).
/// `None` if there's no word at all, only punctuation.
pub fn fts_query(input: &str) -> Option<String> {
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
    fn columns_are_found() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE feeds (id INTEGER PRIMARY KEY, title TEXT)").unwrap();
        assert!(has_column(&conn, "feeds", "title").unwrap());
        assert!(!has_column(&conn, "feeds", "sort_order").unwrap());
        assert!(!has_column(&conn, "folders", "id").unwrap());
        assert_eq!(schema_version(&conn).unwrap(), 0);
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
}
