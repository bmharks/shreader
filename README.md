# Shreader

An RSS and Atom feed reader for Linux that keeps every story it receives.

Most feed readers show what a feed contains right now, so when a site trims its feed to the latest 20 posts, older stories disappear. Shreader keeps every story it downloads in a local, searchable archive, even after it drops out of the feed.

## Features

- **A permanent archive.** Stories stay after they leave the feed, along with whether you've read or starred them.
- **Folders.** Group feeds into folders and drag feeds and folders to rearrange them. Collapsed folders stay collapsed, and the app opens on your top folder.
- **Full-text search** across titles, story text and authors of everything archived.
- **Automatic refresh** every 30 minutes by default (the same as Akregator), adjustable in Settings. Feeds also refresh when the app starts.
- **Images in stories** load as you scroll to them, or only when you click, if you prefer (see [Privacy](#privacy)).
- **OPML import and export**, keeping your folders, so you can move from or to other readers such as Akregator.
- **Keyboard navigation** (see below).

## Installing

Shreader is built from source. You need [Rust](https://rustup.rs) 1.89 or newer and a C compiler (for the bundled SQLite).

```bash
git clone https://github.com/bmharks/shreader.git
cd shreader
packaging/install.sh
```

This builds Shreader and installs it for your user only (no root needed): the program in `~/.local/bin`, plus a launcher and icon so it appears in your applications menu. `~/.local/bin` needs to be on your `PATH`; the script tells you if it isn't.

To uninstall, run `packaging/install.sh --uninstall`. Your archive is left in place (see [Your data](#your-data)).

## Using Shreader

- **Add a feed** with **+ Feed** above the feed list. Paste the feed's address; Shreader looks it up and fills in its name.
- **Right-click** a feed or folder for more: refresh it, mark it read or unread, rename or delete it, or move a feed to another folder.
- **Drag** feeds by their handle into another folder or position, and folders to reorder them.
- **Settings** (the gear icon) has automatic refresh, image loading, OPML import and export, and clearing archived stories.

| Key | Action |
|---|---|
| Right / Left | Next / previous story |
| Up / Down | Scroll the open story |
| Esc | Close a menu or dialog |

Searches match the words you type as an exact phrase.

## Your data

Everything is stored in one SQLite file, readable only by your user account:

```text
~/.local/share/shreader/shreader.db
```

To back it up, copy that file while Shreader is closed. To keep it somewhere else, set the `SHREADER_DB` environment variable to the path of the file you want.

If you used Shreader while it was still called "rss_reader", your archive is moved from `~/.local/share/rss_reader/` the first time you start this version.

Nothing is ever deleted by a refresh. Stories only leave the archive when you delete their feed or clear stories in Settings (where you can choose to keep starred ones).

### Limits

To stop a broken or hostile feed from making the app unusable or filling your disk, Shreader stores:

- stories up to 512 KB each (anything longer is shortened, with a note to open the rest in your browser) and titles up to 500 characters;
- at most 1,000 new stories per feed in one refresh, and 3,000 stories or 100 MB per feed per day. Stories held back arrive in later refreshes if the feed still lists them, and the refresh message says when this happens.

Ordinary feeds stay far below these limits.

## Privacy

- **Feeds** are downloaded directly from each site, which sees your IP address and when you refresh. Shreader identifies itself as `Shreader/<version> (+https://github.com/bmharks/shreader)`.
- **Images** are downloaded from wherever the story says they are. Newsletters often use this to tell when a story was opened. To prevent that, turn off **Load images in stories automatically** in Settings; each image then has a button to load it.
- **Your network:** feeds with public addresses, and all story images, can only be downloaded from public internet addresses, so a feed can't make Shreader send requests to your router or other devices on your network. Feeds you subscribe to at a local address (such as `http://nas/feed` or `http://192.168.1.10/rss`) still work.
- **Links** open in your browser only if they're web links (`http` or `https`).

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

## License

MIT; see [LICENSE](LICENSE).
