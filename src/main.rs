pub mod app;
pub mod context_menu;
pub mod db;
pub mod feed;
pub mod follow_cursor;
pub mod icons;
pub mod models;
pub mod on_escape;
pub mod opml;
pub mod spinner;
pub mod split;

use std::env;
use std::fs;
use anyhow::Result;

use app::RssApp;
use db::Database;
use opml::{export_opml_from_db, import_opml_into_db};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing/logging for background operations
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();

    // Check if CLI flags were provided
    if args.len() > 1 {
        let db = Database::open_default()?;

        match args[1].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--import-opml" => {
                if args.len() < 3 {
                    eprintln!("Usage: rss_reader --import-opml <path-to-opml-file>");
                    std::process::exit(1);
                }
                let file_path = &args[2];
                let content = fs::read_to_string(file_path)?;
                let count = import_opml_into_db(&content, &db)?;
                println!("Successfully imported {} feeds from {:?}", count, file_path);
                return Ok(());
            }
            "--export-opml" => {
                let opml_xml = export_opml_from_db(&db)?;
                if args.len() >= 3 {
                    let out_path = &args[2];
                    fs::write(out_path, &opml_xml)?;
                    println!("Exported subscriptions to {:?}", out_path);
                } else {
                    println!("{}", opml_xml);
                }
                return Ok(());
            }
            "--create-folder" => {
                if args.len() < 3 {
                    eprintln!("Usage: rss_reader --create-folder <folder-name>");
                    std::process::exit(1);
                }
                let folder_name = &args[2];
                let id = db.get_or_create_folder(folder_name)?;
                println!("Created folder '{}' (ID #{})", folder_name, id);
                return Ok(());
            }
            "--add-feed" => {
                if args.len() < 3 {
                    eprintln!("Usage: rss_reader --add-feed <feed-url> [title] [folder-name]");
                    std::process::exit(1);
                }
                let url = &args[2];
                let title = args.get(3).map(|s| s.as_str()).unwrap_or(url);
                let folder_id = if let Some(folder_name) = args.get(4) {
                    Some(db.get_or_create_folder(folder_name)?)
                } else {
                    None
                };

                let id = db.add_or_update_feed(url, title, None, None, folder_id)?;
                println!("Added feed ID #{} ({})", id, title);

                // Fetch immediately
                let fetcher = feed::FeedFetcher::new();
                println!("Syncing feed...");
                let summary = fetcher.fetch_and_sync(&db, id).await?;
                println!(
                    "Sync finished: {} new, {} updated, {} unchanged.",
                    summary.articles_inserted, summary.articles_updated, summary.articles_unchanged
                );
                return Ok(());
            }
            "--move-feed" => {
                if args.len() < 3 {
                    eprintln!("Usage: rss_reader --move-feed <feed-id> [folder-name]");
                    std::process::exit(1);
                }
                let feed_id: i64 = args[2].parse().expect("Invalid feed ID");
                let folder_id = if let Some(folder_name) = args.get(3) {
                    if folder_name.eq_ignore_ascii_case("none") || folder_name.is_empty() {
                        None
                    } else {
                        Some(db.get_or_create_folder(folder_name)?)
                    }
                } else {
                    None
                };

                db.set_feed_folder(feed_id, folder_id)?;
                println!("Moved feed #{} to folder {:?}", feed_id, args.get(3));
                return Ok(());
            }
            "--list-folders" => {
                let folders = db.get_folders()?;
                println!("\nFolders ({} total):", folders.len());
                for f in folders {
                    println!("  📁 [#{}] {} - {} feeds, {} unread / {} total stories", f.id, f.name, f.feed_count, f.unread_count, f.total_count);
                }
                println!();
                return Ok(());
            }
            "--list-feeds" => {
                let folders = db.get_folders()?;
                let feeds = db.get_feeds()?;

                println!("\nSubscribed Feeds ({} total across {} folders):", feeds.len(), folders.len());

                // Folders and their feeds (Akregator style)
                for fld in &folders {
                    println!("\n📁 {} ({} feeds, {} unread):", fld.name, fld.feed_count, fld.unread_count);
                    let in_folder: Vec<_> = feeds.iter().filter(|f| f.folder_id == Some(fld.id)).collect();
                    if in_folder.is_empty() {
                        println!("    (empty)");
                    } else {
                        for f in in_folder {
                            println!("    ↳ [#{}] {} ({}) - {} unread / {} total", f.id, f.title, f.url, f.unread_count, f.total_count);
                        }
                    }
                }

                // Uncategorized
                let uncategorized: Vec<_> = feeds.iter().filter(|f| f.folder_id.is_none()).collect();
                if !uncategorized.is_empty() {
                    println!("\nUncategorized Feeds ({}):", uncategorized.len());
                    for f in uncategorized {
                        println!("  [#{}] {} ({}) - {} unread / {} total", f.id, f.title, f.url, f.unread_count, f.total_count);
                    }
                }
                println!();
                return Ok(());
            }
            "--stats" => {
                let (feeds, articles, unread) = db.get_total_stats()?;
                let folders = db.get_folders()?.len();
                let path = db.path();
                println!("\nRSS Archive Database Statistics:");
                println!("  Database path: {:?}", path);
                println!("  Total Folders: {}", folders);
                println!("  Total Feeds: {}", feeds);
                println!("  Total Stories Archived in SQL: {}", articles);
                println!("  Unread Stories: {}", unread);
                println!();
                return Ok(());
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                print_help();
                std::process::exit(1);
            }
        }
    }

    // Launch Desktop GUI (Iced)
    let settings = iced::application("RSS Archive Reader", RssApp::update, RssApp::view)
        .subscription(RssApp::subscription)
        .theme(RssApp::theme)
        .window(iced::window::Settings {
            size: iced::Size::new(1200.0, 750.0),
            min_size: Some(iced::Size::new(900.0, 550.0)),
            ..Default::default()
        });

    if let Err(e) = settings.run_with(RssApp::new) {
        eprintln!("Failed to launch GUI: {}", e);
    }

    Ok(())
}

fn print_help() {
    println!(
        r#"
RSS Archive Reader (Linux)
A permanent-archive RSS reader backed by SQLite with Akregator-style folder grouping.

USAGE:
    rss_reader                               Launch the Desktop GUI
    rss_reader --import-opml <file>          Import feeds from an OPML file (preserves folders)
    rss_reader --export-opml [file]          Export subscriptions to OPML with folder tags
    rss_reader --create-folder <name>        Create a new folder category
    rss_reader --add-feed <url> [name] [fld] Add and immediately fetch a feed
    rss_reader --move-feed <id> [folder]     Move a feed to a folder (or "none")
    rss_reader --list-folders                List all folders and unread counts
    rss_reader --list-feeds                  List all subscribed feeds grouped by folder
    rss_reader --stats                       Show SQL database storage statistics
    rss_reader --help                        Display this help menu
"#
    );
}
