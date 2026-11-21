pub mod app;
pub mod context_menu;
pub mod db;
pub mod desktop;
pub mod feed;
pub mod follow_cursor;
pub mod key_press;
pub mod lazy_load;
pub mod icons;
pub mod image_space;
pub mod models;
pub mod on_escape;
pub mod opml;
pub mod spinner;
pub mod story_content;
pub mod story_text;
pub mod split;

use std::env;
use std::fs;
use std::path::PathBuf;
use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, error::ErrorKind};

use app::RssApp;
use db::Database;
use opml::{export_opml_from_db, import_opml_into_db};

/// Shreader RSS Reader: an RSS and Atom reader for Linux that keeps every
/// story it receives. With no options, it opens the desktop app.
#[derive(Parser)]
#[command(
    version,
    after_help = "The archive is kept in ~/.local/share/shreader/shreader.db; set SHREADER_DB to use another file."
)]
// One command at a time
#[group(multiple = false)]
struct Cli {
    /// Import feeds from an OPML file (keeps its folders)
    #[arg(long, value_name = "FILE")]
    import_opml: Option<PathBuf>,

    /// Export subscriptions as OPML, with their folders, to FILE (or print them)
    #[arg(long, value_name = "FILE", num_args = 0..=1)]
    export_opml: Option<Option<PathBuf>>,

    /// Create a folder
    #[arg(long, value_name = "NAME")]
    create_folder: Option<String>,

    /// Subscribe to a feed, optionally naming it and putting it in a folder, and download its stories
    #[arg(long, value_names = ["URL", "NAME", "FOLDER"], num_args = 1..=3)]
    add_feed: Option<Vec<String>>,

    /// Move a feed (by its ID, from --list-feeds) to a folder, or out of folders with "none" or no folder
    #[arg(long, value_names = ["ID", "FOLDER"], num_args = 1..=2)]
    move_feed: Option<Vec<String>>,

    /// List folders, with their feed and unread counts
    #[arg(long)]
    list_folders: bool,

    /// List feeds grouped by folder, with their IDs
    #[arg(long)]
    list_feeds: bool,

    /// Show the archive's location and how much it holds
    #[arg(long)]
    stats: bool,
}

impl Cli {
    /// Whether a command was given (otherwise the desktop app opens)
    fn has_command(&self) -> bool {
        self.import_opml.is_some()
            || self.export_opml.is_some()
            || self.create_folder.is_some()
            || self.add_feed.is_some()
            || self.move_feed.is_some()
            || self.list_folders
            || self.list_feeds
            || self.stats
    }
}

fn main() -> Result<()> {
    // Before anything else, so --help, --version and mistyped options
    // don't touch the archive or the GPU
    let cli = Cli::parse();

    // Draw on the high-performance GPU. iced otherwise asks for the
    // low-power one, which on a desktop with both an integrated and a
    // discrete GPU is the integrated GPU even when the monitors are on the
    // discrete card; the compositor then has to bring every frame across
    // from the other GPU, which can make the window and its title bar
    // flicker or show through. Setting WGPU_POWER_PREF yourself overrides
    // this (e.g. WGPU_POWER_PREF=low on a laptop).
    if env::var_os("WGPU_POWER_PREF").is_none() {
        // SAFETY: no other threads exist yet; the async runtime starts below
        unsafe { env::set_var("WGPU_POWER_PREF", "high") };
    }

    // Only the desktop app draws anything (command-line options don't)
    if !cli.has_command() && env::var_os("ICED_PRESENT_MODE").is_none() && supports_mailbox() {
        // SAFETY: as above, still single-threaded
        unsafe { env::set_var("ICED_PRESENT_MODE", "mailbox") };
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

/// Whether frames can be presented in "mailbox" mode, which keeps resizing
/// smooth on Wayland. iced's default waits for each display refresh before
/// it can draw the next frame, and during an interactive resize those waits
/// pile up (sluggish resizing, "Timeout when presenting surface").
///
/// iced only takes the mode from `ICED_PRESENT_MODE`, before the window
/// exists, and fails to start if the driver lacks the mode. So: only when a
/// real GPU has a Vulkan driver, which wgpu then uses, and whose window
/// systems all offer mailbox (Mesa's AMD and Intel drivers, NVIDIA's). The
/// OpenGL fallback doesn't, and keeps the default. Setting
/// ICED_PRESENT_MODE yourself (e.g. to "vsync") overrides this.
fn supports_mailbox() -> bool {
    // A backend chosen by the user that isn't Vulkan
    if let Some(backends) = wgpu::util::backend_bits_from_env() {
        if !backends.contains(wgpu::Backends::VULKAN) {
            return false;
        }
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    instance.enumerate_adapters(wgpu::Backends::VULKAN).iter().any(|adapter| {
        matches!(
            adapter.get_info().device_type,
            wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::VirtualGpu
        )
    })
}

async fn run(cli: Cli) -> Result<()> {
    // Initialize tracing/logging for background operations
    tracing_subscriber::fmt::init();

    if cli.has_command() {
        return run_command(cli).await;
    }

    // Launch Desktop GUI (Iced)
    // One copy at a time: a second one (say, from the menu while the first
    // runs in the tray) shows the first one's window instead
    if let desktop::Instance::AlreadyRunning = desktop::claim_single_instance().await {
        return Ok(());
    }

    let db = match Database::open_default() {
        Ok(db) => db,
        Err(e) => {
            let reason = format!("{:#}", e);
            eprintln!("Couldn't open the archive: {}", reason);
            show_startup_error(reason);
            return Ok(());
        }
    };

    // A daemon rather than a plain application: it keeps running (and
    // refreshing) with no window open, from the tray
    let tray = desktop::start_tray().await;
    let app = iced::daemon(RssApp::title, RssApp::update, RssApp::view)
        .subscription(RssApp::subscription)
        .theme(RssApp::theme);

    if let Err(e) = app.run_with(move || RssApp::new(db, tray)) {
        eprintln!("Failed to launch GUI: {}", e);
    }

    Ok(())
}

/// A small window explaining why the archive couldn't be opened, in place of
/// the main window. (A terminal message alone would go unseen when the app
/// is started from a launcher.)
fn show_startup_error(reason: String) {
    use iced::widget::{button, column, container, text};
    use iced::{Element, Length, Task, Theme};

    #[derive(Debug, Clone)]
    struct Close;

    let message = format!(
        "Shreader couldn't open your story archive, so it won't start.\n\n\
         {}\n\n\
         If another copy of Shreader is running, close it and try again.",
        reason
    );
    fn update(_: &mut String, _: Close) -> Task<Close> {
        iced::exit()
    }
    fn view(message: &String) -> Element<'_, Close> {
        container(
            column![
                text(message.as_str()).size(14),
                button(text("Close")).padding([4, 14]).on_press(Close),
            ]
            .spacing(16),
        )
        .padding(20)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    let result = iced::application("Shreader RSS Reader", update, view)
    .theme(|_| Theme::Dark)
    .window(iced::window::Settings {
        // Unlike the main window, closing this one simply ends the app
        exit_on_close_request: true,
        ..desktop::window_settings(iced::Size::new(560.0, 240.0), None)
    })
    .run_with(move || (message, Task::none()));
    if let Err(e) = result {
        eprintln!("Failed to show the error window: {}", e);
    }
}

/// Carries out a command-line option (only one is given; clap checks that)
async fn run_command(cli: Cli) -> Result<()> {
    let db = Database::open_default()?;

    if let Some(file_path) = cli.import_opml {
        let content = fs::read_to_string(&file_path)
            .with_context(|| format!("Couldn't read {}", file_path.display()))?;
        let count = import_opml_into_db(&content, &db)?;
        println!("Successfully imported {} feeds from {:?}", count, file_path);
    } else if let Some(out_path) = cli.export_opml {
        let opml_xml = export_opml_from_db(&db)?;
        if let Some(out_path) = out_path {
            fs::write(&out_path, &opml_xml)
                .with_context(|| format!("Couldn't write {}", out_path.display()))?;
            println!("Exported subscriptions to {:?}", out_path);
        } else {
            println!("{}", opml_xml);
        }
    } else if let Some(folder_name) = cli.create_folder {
        let id = db.get_or_create_folder(&folder_name)?;
        println!("Created folder '{}' (ID #{})", folder_name, id);
    } else if let Some(args) = cli.add_feed {
        let url = &args[0];
        let title = args.get(1).unwrap_or(url);
        let folder_id = match args.get(2) {
            Some(folder_name) => Some(db.get_or_create_folder(folder_name)?),
            None => None,
        };

        let id = db.add_or_update_feed(url, title, None, None, folder_id)?;
        println!("Added feed ID #{} ({})", id, title);

        // Fetch immediately
        let fetcher = feed::FeedFetcher::new();
        println!("Syncing feed...");
        let summary = fetcher.fetch_and_sync(&db, id).await;
        if let Some(error) = &summary.error {
            eprintln!("Fetching the feed failed: {}", error);
        }
        println!(
            "Sync finished: {} new, {} updated, {} unchanged.",
            summary.articles_inserted, summary.articles_updated, summary.articles_unchanged
        );
        if summary.limited {
            println!(
                "Held back some new stories (more than a feed may add at once); later refreshes pick them up if the feed still lists them."
            );
        }
    } else if let Some(args) = cli.move_feed {
        let Ok(feed_id) = args[0].parse::<i64>() else {
            Cli::command()
                .error(ErrorKind::InvalidValue, format!("'{}' isn't a feed ID (see --list-feeds)", args[0]))
                .exit();
        };
        let folder_id = match args.get(1) {
            Some(name) if !name.is_empty() && !name.eq_ignore_ascii_case("none") => {
                Some(db.get_or_create_folder(name)?)
            }
            _ => None,
        };

        db.set_feed_folder(feed_id, folder_id)?;
        println!("Moved feed #{} to folder {:?}", feed_id, args.get(1));
    } else if cli.list_folders {
        let folders = db.get_folders()?;
        println!("\nFolders ({} total):", folders.len());
        for f in folders {
            println!("  📁 [#{}] {} - {} feeds, {} unread / {} total stories", f.id, f.name, f.feed_count, f.unread_count, f.total_count);
        }
        println!();
    } else if cli.list_feeds {
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
    } else if cli.stats {
        let (feeds, articles, unread) = db.get_total_stats()?;
        let folders = db.get_folders()?.len();
        println!("\nRSS Archive Database Statistics:");
        println!("  Database path: {:?}", db.path());
        println!("  Total Folders: {}", folders);
        println!("  Total Feeds: {}", feeds);
        println!("  Total Stories Archived in SQL: {}", articles);
        println!("  Unread Stories: {}", unread);
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_options_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn one_command_at_a_time() {
        assert!(Cli::try_parse_from(["shreader", "--stats", "--list-feeds"]).is_err());
        assert!(Cli::try_parse_from(["shreader", "--stats", "extra"]).is_err());
        assert!(Cli::try_parse_from(["shreader", "--bogus"]).is_err());
        assert!(!Cli::try_parse_from(["shreader"]).unwrap().has_command());
    }

    #[test]
    fn optional_values() {
        let cli = Cli::try_parse_from(["shreader", "--export-opml"]).unwrap();
        assert_eq!(cli.export_opml, Some(None));
        let cli = Cli::try_parse_from(["shreader", "--add-feed", "https://example.com/feed", "Example", "News"]).unwrap();
        assert_eq!(cli.add_feed.unwrap().len(), 3);
        assert!(Cli::try_parse_from(["shreader", "--add-feed"]).is_err());
    }
}
