# RSS Archive Reader (Linux / Rust)

A modern, high-performance RSS/Atom feed reader built in Rust for Linux, designed around one core principle: **you should never lose a story**.

Most RSS readers mirror the current state of a feed, which means when a publisher trims their feed to the newest 10–20 posts, older stories quietly vanish. **RSS Archive Reader** implements an append-only SQLite storage engine with full-text search, keeping every story you ever receive permanently archived locally.

---

## Key Features & Archival Guarantees

- **Permanent SQL Archival**:
  - Stories are stored in SQLite in **Write-Ahead Logging (WAL)** mode for crash resilience and zero data corruption.
  - Periodic feed syncs **never delete** older articles. If an article drops out of a publisher's feed, it remains preserved in your local SQL database forever.
- **Akregator-Style Folder Organization**:
  - Group feeds into custom categories and folders (e.g., `Linux & Open Source`, `Technology`, `News`).
  - **Drag-and-Drop Organization**: Drag feeds by their `⠿` handle directly into any folder or out into "Uncategorized" to reorganize your subscriptions on the fly.
  - **Instant Naming Focus**: Clicking `+ Folder` immediately focuses and highlights the input box so you can start typing the folder name right away without clicking into it.
  - **Aggregated Reading**: Clicking a folder displays articles from **all feeds inside that folder** with combined unread counts.
  - **Collapsible Tree**: Expand (`▼`) or collapse (`▶`) folders in the sidebar.
  - **Safe Deletion**: Deleting a folder unlinks its feeds safely without deleting the feeds or their archived stories.
- **Robust Deduplication Engine**:
  - Normalizes URLs by stripping tracking query parameters (`utm_*`, `ref`, `fbclid`, `gclid`).
  - Computes a canonical SHA-256 hash using `(feed_id, guid)`, with fallback to `(feed_id, clean_url)` or `(feed_id, title + content)`.
  - Updating an article modifies its content or metadata without ever wiping your read or starred bookmark status.
- **SQLite FTS5 Full-Text Search**:
  - Automated triggers keep an SQLite FTS5 index synchronized across article titles, summaries, clean body text, and authors.
  - Search through thousands of archived stories instantly without network delays.
- **Native Desktop GUI (Iced)**:
  - 3-column desktop layout (Feeds & Folders Tree, Article List, Full Reader View).
  - Dark theme with unread badges, star bookmarks, and direct "Open in Browser" integration (`xdg-open`).
  - Live status indicator displaying the number of stories and folders safely archived in SQL.
- **OPML 2.0 Import & Export with Folders**:
  - Compatible with Akregator, Feedly, and NetNewsWire OPML files with nested `<outline>` tags.
  - Import or export subscriptions while preserving your complete folder hierarchy.

---

## Project Structure

```text
├── Cargo.toml
├── sample_feeds.opml          # Example OPML file for testing
└── src
    ├── app.rs                 # Desktop GUI (Iced 0.13 Elm architecture) with folder tree
    ├── db.rs                  # SQLite engine with WAL, FTS5, folders & append-only upsert
    ├── feed.rs                # Feed fetcher (reqwest + feed-rs) with conditional GET & hashing
    ├── main.rs                # App entrypoint with CLI subcommands & GUI launcher
    ├── models.rs              # Domain data models (Folder, Feed, Article, Filters)
    └── opml.rs                # OPML 2.0 importer & exporter with nested folder support
```

---

## Building and Running

### Prerequisites
- Rust 1.80+ (Rust 2024 edition supported)
- Standard Linux libraries (Wayland / X11 / Mesa)

### Launch Desktop GUI
```bash
cargo run
```

### CLI Subcommands

| Command | Description |
|---|---|
| `cargo run -- --stats` | View total feeds, folders, archived story count, and database path |
| `cargo run -- --list-folders` | List all folders with feed counts and unread badges |
| `cargo run -- --list-feeds` | List all feeds grouped by folder with unread story counts |
| `cargo run -- --create-folder <name>` | Create a new folder category |
| `cargo run -- --move-feed <id> [folder]` | Move a feed to a folder (or "none" for uncategorized) |
| `cargo run -- --add-feed <url> [name] [fld]` | Add and immediately ingest a feed into a folder |
| `cargo run -- --import-opml <file.opml>` | Import subscriptions from an OPML file (preserves folders) |
| `cargo run -- --export-opml [out.opml]` | Export subscriptions to OPML with nested folder outlines |
| `cargo run -- --help` | Display CLI help menu |

---

## Database Location & Configuration

By default, the database is placed according to Linux XDG standards:
```text
~/.local/share/rss_reader/rss_reader.db
```

You can customize the storage path at any time by setting the `RSS_READER_DB` environment variable:
```bash
export RSS_READER_DB=/path/to/custom_storage.db
cargo run
```

If the XDG directory is not writable, the application automatically falls back to `./rss_reader.db`.

---

## Testing

Run the automated test suite verifying database append-only persistence, folder grouping, tracking parameter cleanup, hash stability, and nested OPML parsing:
```bash
cargo test
```
