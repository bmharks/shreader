# Shreader (Pronounced Shredder)

An RSS and Atom feed reader for Linux that keeps every story it receives.

Shreader puts an emphasis on simplicity and never losing a story. It keeps every story it downloads in a local, searchable archive via SQLite, even after it drops out of the feed. It does not include a web-browser or any of the other bloat that other RSS readers have included as of late. It is made to my tastes, but has an easily accessible interface with dragging, right click contexts, and other niceties that somebody may want.

Part of the reason I wanted to write this one from scratch was to include story grouping/deduplication for similar or further developing stories from multiple sources. Think Ground News but for everything in your personal interests. More updates on this soon.
It will also have a light mode. At some point.

## Installing

Shreader is built from source. You need [Rust](https://rustup.rs) 1.89 or newer and a C compiler (for the bundled SQLite).

```bash
git clone https://github.com/bmharks/shreader.git
cd shreader
packaging/install.sh
```

This builds Shreader and installs it for your user only (no root needed): the program in `~/.local/bin`, plus a launcher and icon so it appears in your applications menu. `~/.local/bin` needs to be on your `PATH`; the script tells you if it isn't.

To uninstall, run `packaging/install.sh --uninstall`. Your archive is left in place.

## Using Shreader

- **Add a feed** with **+ Feed** above the feed list. Paste the feed's address; Shreader looks it up and fills in its name.
- **Right-click** a feed or folder for more: refresh it, mark it read or unread, rename or delete it, or move a feed to another folder.
- **Drag** feeds by their handle into another folder or position, and folders to reorder them.
- **Settings** (the gear icon) has automatic refresh, image loading, OPML import and export, and clearing archived stories.
- **Closing the window** keeps Shreader running in the system tray, so feeds keep refreshing. Click the tray icon to open it again; quit from the tray icon's menu or with Ctrl+Q. (On desktops without a system tray, closing the window quits.)

| Key | Action |
|---|---|
| Right / Left | Next / previous story |
| Up / Down | Scroll the open story |
| Ctrl+F | Find in the open story |
| F3 / Shift+F3 | Next / previous match |
| Ctrl+Q | Quit |
| Esc | Close a menu, dialog or the find bar |

Searches match the words you type as an exact phrase.

## Command line

The same program also works from a terminal:

| Command | What it does |
|---|---|
| `shreader --stats` | Show the archive's location and how many feeds, folders and stories it holds |
| `shreader --list-folders` | List folders with their unread counts |
| `shreader --list-feeds` | List feeds grouped by folder, with their IDs |
| `shreader --create-folder <name>` | Create a folder |
| `shreader --add-feed <url> [name] [folder]` | Subscribe to a feed and download its stories |
| `shreader --move-feed <id> [folder]` | Move a feed to a folder (or `none`) |
| `shreader --import-opml <file>` | Import feeds from an OPML file |
| `shreader --export-opml [file]` | Export your feeds and folders to OPML |
| `shreader --help` | Show this list |

## Building and testing

```bash
cargo build --release --locked   # the program is target/release/shreader
cargo test
```

The code is in `src/`: `app.rs` is the interface (built with [iced](https://iced.rs)), `db.rs` the SQLite archive, `feed.rs` downloading and processing feeds, `opml.rs` import and export, and `story_content.rs` turning story HTML into text and images. `packaging/` has the launcher, icon and install script.

Everything is stored in one SQLite file, readable only by your user account:

```text
~/.local/share/shreader/shreader.db
```

To back it up, copy that file while Shreader isn't running (quit it from the tray icon or with Ctrl+Q; closing the window leaves it running). To keep it somewhere else, set the `SHREADER_DB` environment variable to the path of the file you want.

## License

MIT; see [LICENSE](LICENSE).
