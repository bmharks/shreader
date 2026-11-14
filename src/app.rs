use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use chrono::{DateTime, Utc};
use iced::mouse;
use iced::widget::{
    button, column, container, horizontal_rule, horizontal_space, mouse_area, pick_list, row,
    scrollable, stack, text, text_editor, text_input, vertical_rule, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Vector};

use crate::db::Database;
use crate::feed::{FeedFetcher, FeedPreview, StoryImage};
use crate::story_content::{self, Block};
use crate::context_menu::context_menu;
use crate::follow_cursor::follow_cursor;
use crate::lazy_load::on_visible;
use crate::icons::{icon, Icon, Tint};
use crate::spinner::spinner;
use crate::split::split;
use crate::on_escape::on_escape;
use crate::models::{Article, ArticleFilter, Feed, Folder, FeedSyncSummary};

const FOLDER_INPUT_ID: &str = "new_folder_input_id";
const RENAME_FEED_INPUT_ID: &str = "rename_feed_input_id";
const EDIT_FEED_TITLE_INPUT_ID: &str = "edit_feed_title_input_id";
const RENAME_FOLDER_INPUT_ID: &str = "rename_folder_input_id";
const ADD_FEED_URL_INPUT_ID: &str = "add_feed_url_input_id";

/// How many article cards to render at a time. Every message rebuilds and
/// re-lays out the whole widget tree, so rendering thousands of cards made each
/// update cost 100+ ms (and made drag-and-drop lag). More load on demand.
const ARTICLE_PAGE_SIZE: usize = 150;

/// How long a status message (toast) stays up before dismissing itself
const STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Story images start downloading once they're within this distance of the
/// visible part of the reader (about a screen ahead)
const IMAGE_PRELOAD_MARGIN: f32 = 1000.0;

/// Story list column width: default, limits, and where it's saved
const ARTICLE_LIST_DEFAULT_WIDTH: f32 = 360.0;
const ARTICLE_LIST_MIN_WIDTH: f32 = 260.0;
const READER_MIN_WIDTH: f32 = 320.0;
/// Widest the story text gets; past this, lines get too long to read comfortably
const READER_TEXT_MAX_WIDTH: f32 = 760.0;
/// Room the story's scrollbar takes on the right (default 10px bar + 4px gap).
/// The headline and footer leave the same room, so all three center alike.
const READER_SCROLLBAR_GUTTER: f32 = 14.0;
const ARTICLE_LIST_WIDTH_SETTING: &str = "article_list_width";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveFilter {
    All,
    Unread,
    Starred,
    Folder(i64),
    Feed(i64),
}

pub struct RssApp {
    db: Database,
    fetcher: Arc<FeedFetcher>,
    folders: Vec<Folder>,
    feeds: Vec<Feed>,
    articles: Vec<Article>,
    article_render_limit: usize,                // how many of `articles` are rendered
    expanded_folders: HashSet<i64>,
    active_filter: ActiveFilter,
    selected_article_id: Option<i64>,
    /// The selected story split into text and images, converted once when
    /// it's opened (too slow to redo on every view rebuild)
    reader_body: Option<(i64, Vec<Block>)>,
    /// Images of the open story, by URL
    story_images: HashMap<String, ImageState>,
    /// The open story's text blocks as read-only editors, so text can be
    /// selected and copied (iced's plain `text` can't be selected)
    reader_texts: Vec<text_editor::Content>,
    /// Text block whose right-click (Copy / Select all) menu is open
    reader_text_menu: Option<usize>,
    search_query: String,
    new_folder_input: String,
    show_new_folder_input: bool,
    is_syncing: bool,
    status_message: Option<(String, bool)>,
    /// Bumped whenever a new status message appears; see `STATUS_TIMEOUT`
    status_generation: u64,
    stats: (i64, i64, i64), // (total_feeds, total_articles, unread_articles)

    // Drag-and-drop state
    dragging_feed: Option<i64>,                 // feed_id being dragged

    // Feed context menu (right click) and in-place rename
    feed_menu: Option<FeedMenu>,
    renaming_feed: Option<(i64, String)>,       // (feed_id, edited title)
    editing_feed: Option<EditFeedDialog>,

    add_feed: Option<AddFeedDialog>,
    /// Story whose right-click menu is open
    article_menu: Option<i64>,
    article_list_width: f32,

    // Folder context menu (right click) and in-place rename
    folder_menu: Option<FolderMenu>,
    renaming_folder: Option<(i64, String)>,     // (folder_id, edited name)
    drag_hover_target: Option<DropTarget>,
}

/// What a dragged feed is currently over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// A folder header (None = the Uncategorized header): move to the end of it
    Folder(Option<i64>),
    /// Another feed row: take that feed's position (reorders within a folder,
    /// or moves into that feed's folder at that spot)
    Feed(i64),
}

/// The open right-click menu for a feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeedMenu {
    feed_id: i64,
    page: FeedMenuPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedMenuPage {
    Main,
    /// Folder list for "Move to…"
    MoveTo,
    /// The "are you sure?" step for Delete
    ConfirmDelete,
}

/// The open right-click menu for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FolderMenu {
    folder_id: i64,
    /// Showing the "are you sure?" step for Delete
    confirm_delete: bool,
}

/// State of the "Edit feed" dialog.
#[derive(Debug, Clone)]
struct EditFeedDialog {
    feed_id: i64,
    title: String,
    url: String,
    error: Option<String>,
}

/// Download state of an image in the open story.
#[derive(Debug, Clone)]
enum ImageState {
    Loading,
    Raster(iced::widget::image::Handle),
    Svg(iced::widget::svg::Handle),
    Failed,
}

/// State of the "Add feed" dialog.
#[derive(Debug, Clone)]
struct AddFeedDialog {
    url: String,
    name: String,
    /// The user typed their own name, so the feed's title no longer overwrites it
    name_edited: bool,
    folder: FolderChoice,
    lookup: FeedLookup,
    /// Bumped on every URL edit; lookups for older URLs are ignored
    lookup_generation: u64,
    error: Option<String>,
}

/// Progress of fetching the typed URL to fill in the feed's name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FeedLookup {
    Idle,
    Pending,
    Found { story_count: usize },
    Failed(String),
}

/// An entry in the Add feed dialog's folder picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChoice {
    id: Option<i64>,
    name: String,
}

impl std::fmt::Display for FolderChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// How long URL typing must pause before the feed is looked up.
const FEED_LOOKUP_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Which edge of a feed row shows the insertion line while dragging over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertEdge {
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
pub enum Message {
    SelectFilter(ActiveFilter),
    SelectArticle(i64),
    FeedAdded(Result<(i64, String), String>),
    RefreshClicked,
    SyncFinished(Result<Vec<FeedSyncSummary>, String>),
    SearchChanged(String),
    ToggleStar(i64),
    ToggleRead(i64),
    MarkAllRead,
    DeleteFeed(i64),
    OpenUrl(String),
    DismissStatus,
    /// The auto-dismiss timer for status message number `.0` ran out
    StatusTimedOut(u64),
    ShowMoreArticles,

    /// Selecting text etc. in story text block `.0` (edits are ignored)
    ReaderTextAction(usize, text_editor::Action),
    OpenReaderTextMenu(usize),
    CloseReaderTextMenu,
    CopyReaderSelection(usize),
    SelectAllReaderText(usize),
    /// A story image scrolled near the visible area: download it
    LoadStoryImage(String),
    /// An image in the open story finished downloading (or failed)
    StoryImageLoaded(String, Result<StoryImage, String>),

    // Divider between the story list and the reader
    ArticleListResized(f32),
    ArticleListResizeEnded,

    // Add feed dialog
    OpenAddFeed,
    AddFeedUrlChanged(String),
    AddFeedNameChanged(String),
    AddFeedFolderSelected(FolderChoice),
    /// The URL stopped changing; fetch it if it's still the current one
    AddFeedLookupDue(u64),
    AddFeedLookupDone(u64, Result<FeedPreview, String>),
    SubmitAddFeed,
    CancelAddFeed,

    // Folder Actions
    ToggleFolderExpand(i64),
    ShowNewFolderInput,
    CancelNewFolderInput,
    NewFolderInputChanged(String),
    CreateFolderClicked,
    DeleteFolder(i64),
    OpenFolderMenu(i64),
    CloseFolderMenu,
    AskDeleteFolder(i64),
    StartRenameFolder(i64),
    RenameFolderInputChanged(String),
    SubmitRenameFolder,
    CancelRenameFolder,

    // Story context menu
    OpenArticleMenu(i64),
    CloseArticleMenu,
    SetArticleRead(i64, bool),
    OpenArticleInBrowser(String),

    // Read state & refresh for one feed or folder (context menus)
    SetFeedRead(i64, bool),
    SetFolderRead(i64, bool),
    RefreshFeed(i64),
    RefreshFolder(i64),
    /// A single feed / folder refresh finished; carries its display name
    PartialRefreshFinished(String, Result<Vec<FeedSyncSummary>, String>),

    // Feed context menu & rename
    OpenFeedMenu(i64),
    CloseFeedMenu,
    AskDeleteFeed(i64),
    ShowMoveToMenu(i64),
    FeedMenuBack(i64),
    MoveFeedTo(i64, Option<i64>),
    OpenEditFeed(i64),
    EditFeedTitleChanged(String),
    EditFeedUrlChanged(String),
    SaveEditFeed,
    CancelEditFeed,
    /// Does nothing; used where an event must be consumed (e.g. a modal backdrop)
    Noop,
    StartRenameFeed(i64),
    RenameFeedInputChanged(String),
    SubmitRenameFeed,
    CancelRenameFeed,
    EscapePressed,

    // Drag-and-Drop Actions
    DragFeedStarted(i64),
    DragOverTarget(DropTarget),
    DragLeaveTarget(DropTarget),
    DropOnTarget(DropTarget),
    DragFeedDropped,
}

impl RssApp {
    pub fn new() -> (Self, Task<Message>) {
        let db = match Database::open_default() {
            Ok(db) => db,
            Err(e) => {
                eprintln!("Failed to open default XDG database: {}. Falling back to ./rss_reader.db", e);
                Database::open(std::path::Path::new("rss_reader.db"))
                    .expect("Failed to initialize SQLite database")
            }
        };

        let fetcher = Arc::new(FeedFetcher::new());

        let mut app = Self {
            db,
            fetcher,
            folders: Vec::new(),
            feeds: Vec::new(),
            articles: Vec::new(),
            article_render_limit: ARTICLE_PAGE_SIZE,
            expanded_folders: HashSet::new(),
            active_filter: ActiveFilter::All,
            selected_article_id: None,
            reader_body: None,
            story_images: HashMap::new(),
            reader_texts: Vec::new(),
            reader_text_menu: None,
            search_query: String::new(),
            new_folder_input: String::new(),
            show_new_folder_input: false,
            is_syncing: false,
            status_message: None,
            status_generation: 0,
            stats: (0, 0, 0),
            dragging_feed: None,
            feed_menu: None,
            renaming_feed: None,
            editing_feed: None,
            add_feed: None,
            article_menu: None,
            article_list_width: ARTICLE_LIST_DEFAULT_WIDTH,
            folder_menu: None,
            renaming_folder: None,
            drag_hover_target: None,
        };

        app.reload_data();

        if let Some(width) = app
            .db
            .get_setting(ARTICLE_LIST_WIDTH_SETTING)
            .ok()
            .flatten()
            .and_then(|w| w.parse::<f32>().ok())
            .filter(|w| w.is_finite())
        {
            app.article_list_width = width;
        }

        // By default, expand all folders so user sees their structure
        for f in &app.folders {
            app.expanded_folders.insert(f.id);
        }

        // If feeds exist, automatically trigger an initial background sync
        let initial_task = if !app.feeds.is_empty() {
            let db_clone = app.db.clone();
            let fetcher_clone = app.fetcher.clone();
            app.is_syncing = true;
            Task::perform(
                async move {
                    fetcher_clone
                        .sync_all_feeds(&db_clone)
                        .await
                        .map_err(|e| e.to_string())
                },
                Message::SyncFinished,
            )
        } else {
            Task::none()
        };

        (app, initial_task)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // No global event listeners: they're fed every mouse move and can overflow
        // their channel ("TrySendError { kind: Full }"). Drag release comes from
        // the floating badge (follow_cursor.rs); Esc for context menus from the
        // menu overlay (context_menu.rs), and for the rename fields and dialogs
        // from `on_escape` wrappers.
        Subscription::none()
    }

    fn reload_data(&mut self) {
        self.folders = self.db.get_folders().unwrap_or_default();
        self.feeds = self.db.get_feeds().unwrap_or_default();
        self.stats = self.db.get_total_stats().unwrap_or((0, 0, 0));
        self.reload_articles();
    }

    fn reload_articles(&mut self) {
        let filter = match self.active_filter {
            ActiveFilter::All => ArticleFilter {
                search_query: if self.search_query.trim().is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                },
                ..Default::default()
            },
            ActiveFilter::Unread => ArticleFilter {
                unread_only: true,
                search_query: if self.search_query.trim().is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                },
                ..Default::default()
            },
            ActiveFilter::Starred => ArticleFilter {
                starred_only: true,
                search_query: if self.search_query.trim().is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                },
                ..Default::default()
            },
            ActiveFilter::Folder(folder_id) => ArticleFilter {
                folder_id: Some(folder_id),
                search_query: if self.search_query.trim().is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                },
                ..Default::default()
            },
            ActiveFilter::Feed(feed_id) => ArticleFilter {
                feed_id: Some(feed_id),
                search_query: if self.search_query.trim().is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                },
                ..Default::default()
            },
        };

        self.articles = self.db.get_articles(&filter).unwrap_or_default();
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let previous_status = self.status_message.clone();
        let task = self.handle_message(message);

        // Any new status message starts its own auto-dismiss timer. The timer only
        // clears the message it was started for, so a newer message gets its own
        // full time on screen.
        if self.status_message.is_some() && self.status_message != previous_status {
            self.status_generation += 1;
            let generation = self.status_generation;
            let timeout = Task::perform(tokio::time::sleep(STATUS_TIMEOUT), move |_| {
                Message::StatusTimedOut(generation)
            });
            return Task::batch([task, timeout]);
        }
        task
    }

    fn handle_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SelectFilter(filter) => {
                self.active_filter = filter;
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_articles();
                Task::none()
            }
            Message::SelectArticle(article_id) => {
                self.selected_article_id = Some(article_id);
                self.mark_opened_story_read(article_id);
                // The list holds no story bodies; load just this one
                let blocks = self
                    .db
                    .get_article(article_id)
                    .ok()
                    .flatten()
                    .map(|article| reader_blocks(&article))
                    .unwrap_or_default();
                self.prune_story_images(&blocks);
                self.reader_texts = blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text(body) => Some(text_editor::Content::with_text(body)),
                        Block::Image { .. } => None,
                    })
                    .collect();
                self.reader_text_menu = None;
                self.reader_body = Some((article_id, blocks));
                Task::none()
            }
            Message::LoadStoryImage(url) => {
                if self.story_images.contains_key(&url) {
                    return Task::none(); // already requested
                }
                self.story_images.insert(url.clone(), ImageState::Loading);
                let fetcher = self.fetcher.clone();
                Task::perform(
                    {
                        let url = url.clone();
                        async move { fetcher.fetch_image(&url).await }
                    },
                    move |result| Message::StoryImageLoaded(url.clone(), result),
                )
            }
            Message::ReaderTextAction(index, action) => {
                // Read-only: allow selecting, moving, clicking, but never editing
                if !action.is_edit() {
                    if let Some(content) = self.reader_texts.get_mut(index) {
                        content.perform(action);
                    }
                }
                Task::none()
            }
            Message::OpenReaderTextMenu(index) => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.reader_text_menu = Some(index);
                Task::none()
            }
            Message::CloseReaderTextMenu => {
                self.reader_text_menu = None;
                Task::none()
            }
            Message::CopyReaderSelection(index) => {
                self.reader_text_menu = None;
                match self.reader_texts.get(index).and_then(|content| content.selection()) {
                    Some(selection) => iced::clipboard::write(selection),
                    None => Task::none(),
                }
            }
            Message::SelectAllReaderText(index) => {
                self.reader_text_menu = None;
                if let Some(content) = self.reader_texts.get_mut(index) {
                    content.perform(text_editor::Action::SelectAll);
                }
                Task::none()
            }
            Message::StoryImageLoaded(url, result) => {
                // Ignore downloads for a story that's no longer open
                if let Some(state) = self.story_images.get_mut(&url) {
                    *state = match result {
                        Ok(StoryImage::Raster { width, height, pixels }) => ImageState::Raster(
                            iced::widget::image::Handle::from_rgba(width, height, pixels),
                        ),
                        Ok(StoryImage::Svg(bytes)) => {
                            ImageState::Svg(iced::widget::svg::Handle::from_memory(bytes))
                        }
                        Err(e) => {
                            tracing::debug!("Story image {} failed: {}", url, e);
                            ImageState::Failed
                        }
                    };
                }
                Task::none()
            }
            Message::FeedAdded(result) => {
                self.is_syncing = false;
                // Reload either way: on a failed first fetch the feed is still added
                self.reload_data();
                match result {
                    Ok((feed_id, url)) => {
                        let name = self
                            .feeds
                            .iter()
                            .find(|f| f.id == feed_id)
                            .map(|f| f.title.clone())
                            .unwrap_or(url);
                        self.status_message = Some((format!("Subscribed to '{}'", name), false));
                    }
                    Err(e) => {
                        self.status_message = Some((e, true));
                    }
                }
                Task::none()
            }
            Message::ArticleListResized(width) => {
                self.article_list_width = width;
                Task::none()
            }
            Message::ArticleListResizeEnded => {
                let _ = self
                    .db
                    .set_setting(ARTICLE_LIST_WIDTH_SETTING, &self.article_list_width.to_string());
                Task::none()
            }
            Message::OpenAddFeed => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                let folder_id = self.default_new_feed_folder();
                self.add_feed = Some(AddFeedDialog {
                    url: String::new(),
                    name: String::new(),
                    name_edited: false,
                    folder: self
                        .folder_choices()
                        .into_iter()
                        .find(|c| c.id == folder_id)
                        .unwrap_or_else(uncategorized_choice),
                    lookup: FeedLookup::Idle,
                    lookup_generation: 0,
                    error: None,
                });
                text_input::focus(text_input::Id::new(ADD_FEED_URL_INPUT_ID))
            }
            Message::AddFeedUrlChanged(value) => {
                let Some(dialog) = &mut self.add_feed else {
                    return Task::none();
                };
                dialog.url = value;
                dialog.error = None;
                dialog.lookup_generation += 1;
                // A name filled in from the previous URL no longer applies
                if !dialog.name_edited {
                    dialog.name.clear();
                }
                let url = normalize_feed_url(&dialog.url);
                if url.as_ref().is_some_and(|url| self.feeds.iter().any(|f| &f.url == url)) {
                    // No point fetching a feed that's already subscribed
                    dialog.lookup = FeedLookup::Idle;
                    dialog.error = Some("You're already subscribed to that URL.".to_string());
                    Task::none()
                } else if url.is_some() {
                    dialog.lookup = FeedLookup::Pending;
                    // Wait for typing to pause rather than fetching on every keystroke
                    let generation = dialog.lookup_generation;
                    Task::perform(tokio::time::sleep(FEED_LOOKUP_DELAY), move |_| {
                        Message::AddFeedLookupDue(generation)
                    })
                } else {
                    dialog.lookup = FeedLookup::Idle;
                    Task::none()
                }
            }
            Message::AddFeedLookupDue(generation) => {
                let Some(dialog) = &self.add_feed else {
                    return Task::none();
                };
                if generation != dialog.lookup_generation {
                    return Task::none(); // URL changed again since; a newer lookup is queued
                }
                let Some(url) = normalize_feed_url(&dialog.url) else {
                    return Task::none();
                };
                let fetcher = self.fetcher.clone();
                Task::perform(async move { fetcher.preview(&url).await }, move |result| {
                    Message::AddFeedLookupDone(generation, result)
                })
            }
            Message::AddFeedLookupDone(generation, result) => {
                let Some(dialog) = &mut self.add_feed else {
                    return Task::none();
                };
                if generation != dialog.lookup_generation {
                    return Task::none();
                }
                match result {
                    Ok(preview) => {
                        dialog.lookup = FeedLookup::Found { story_count: preview.story_count };
                        if !dialog.name_edited {
                            dialog.name = preview.title.unwrap_or_default();
                        }
                    }
                    Err(e) => dialog.lookup = FeedLookup::Failed(e),
                }
                Task::none()
            }
            Message::AddFeedNameChanged(value) => {
                if let Some(dialog) = &mut self.add_feed {
                    // Clearing the name hands it back to the auto-fill
                    dialog.name_edited = !value.trim().is_empty();
                    dialog.name = value;
                    dialog.error = None;
                }
                Task::none()
            }
            Message::AddFeedFolderSelected(choice) => {
                if let Some(dialog) = &mut self.add_feed {
                    dialog.folder = choice;
                }
                Task::none()
            }
            Message::SubmitAddFeed => {
                let Some(dialog) = &mut self.add_feed else {
                    return Task::none();
                };
                let Some(url) = normalize_feed_url(&dialog.url) else {
                    dialog.error = Some(
                        "Enter a feed address, e.g. https://example.com/feed.xml".to_string(),
                    );
                    return Task::none();
                };
                if self.feeds.iter().any(|f| f.url == url) {
                    dialog.error = Some("You're already subscribed to that URL.".to_string());
                    return Task::none();
                }
                let name = dialog.name.trim().to_string();
                let folder_id = dialog.folder.id;
                self.add_feed = None;
                if let Some(id) = folder_id {
                    self.expanded_folders.insert(id);
                }
                // An empty name lets the first sync fill in the feed's own title
                self.subscribe(url, (!name.is_empty()).then_some(name), folder_id)
            }
            Message::CancelAddFeed => {
                self.add_feed = None;
                Task::none()
            }
            Message::RefreshClicked => {
                if self.is_syncing {
                    return Task::none();
                }
                self.is_syncing = true;
                let db_clone = self.db.clone();
                let fetcher_clone = self.fetcher.clone();

                Task::perform(
                    async move {
                        fetcher_clone
                            .sync_all_feeds(&db_clone)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::SyncFinished,
                )
            }
            Message::SyncFinished(result) => {
                self.is_syncing = false;
                match result {
                    Ok(summaries) => {
                        let total_inserted: usize = summaries.iter().map(|s| s.articles_inserted).sum();
                        let total_updated: usize = summaries.iter().map(|s| s.articles_updated).sum();
                        self.status_message = Some((
                            format!(
                                "Sync complete: {} new stories archived, {} updated.",
                                total_inserted, total_updated
                            ),
                            false,
                        ));
                        self.reload_data();
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Sync failed: {}", e), true));
                    }
                }
                Task::none()
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_articles();
                Task::none()
            }
            Message::ToggleStar(article_id) => {
                let _ = self.db.toggle_article_starred(article_id);
                self.reload_data();
                Task::none()
            }
            Message::ToggleRead(article_id) => {
                if let Ok(Some(art)) = self.db.get_article(article_id) {
                    let _ = self.db.set_article_read(article_id, !art.is_read);
                    self.reload_data();
                }
                Task::none()
            }
            Message::OpenArticleMenu(article_id) => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.reader_text_menu = None;
                self.article_menu = Some(article_id);
                Task::none()
            }
            Message::CloseArticleMenu => {
                self.article_menu = None;
                Task::none()
            }
            Message::SetArticleRead(article_id, is_read) => {
                self.article_menu = None;
                if let Err(e) = self.db.set_article_read(article_id, is_read) {
                    self.status_message = Some((format!("Failed to update story: {}", e), true));
                }
                self.reload_data();
                Task::none()
            }
            Message::OpenArticleInBrowser(url) => {
                self.article_menu = None;
                Task::done(Message::OpenUrl(url))
            }
            Message::SetFeedRead(feed_id, is_read) => {
                self.feed_menu = None;
                if let Err(e) = self.db.set_articles_read_state(Some(feed_id), None, is_read) {
                    self.status_message = Some((format!("Failed to update stories: {}", e), true));
                }
                self.reload_data();
                Task::none()
            }
            Message::SetFolderRead(folder_id, is_read) => {
                self.folder_menu = None;
                if let Err(e) = self.db.set_articles_read_state(None, Some(folder_id), is_read) {
                    self.status_message = Some((format!("Failed to update stories: {}", e), true));
                }
                self.reload_data();
                Task::none()
            }
            Message::RefreshFeed(feed_id) => {
                self.feed_menu = None;
                let name = self
                    .feeds
                    .iter()
                    .find(|f| f.id == feed_id)
                    .map(|f| format!("'{}'", f.title))
                    .unwrap_or_else(|| "feed".to_string());
                self.refresh_feeds(name, vec![feed_id])
            }
            Message::RefreshFolder(folder_id) => {
                self.folder_menu = None;
                let name = self
                    .folders
                    .iter()
                    .find(|f| f.id == folder_id)
                    .map(|f| format!("'{}'", f.name))
                    .unwrap_or_else(|| "folder".to_string());
                let feed_ids = self.feed_ids_in(Some(folder_id));
                self.refresh_feeds(name, feed_ids)
            }
            Message::PartialRefreshFinished(name, result) => {
                self.is_syncing = false;
                self.reload_data();
                self.status_message = Some(match result {
                    Ok(summaries) => {
                        let inserted: usize = summaries.iter().map(|s| s.articles_inserted).sum();
                        let failed: Vec<&str> = summaries
                            .iter()
                            .filter(|s| s.error.is_some())
                            .map(|s| s.feed_title.as_str())
                            .collect();
                        let new_stories = match inserted {
                            0 => "no new stories".to_string(),
                            1 => "1 new story".to_string(),
                            n => format!("{} new stories", n),
                        };
                        match (failed.len(), summaries.len()) {
                            (0, _) => (format!("Refreshed {}: {}", name, new_stories), false),
                            // A single feed: say why it failed
                            (_, 1) => (
                                format!(
                                    "Couldn't refresh {}: {}",
                                    name,
                                    summaries[0].error.as_deref().unwrap_or("unknown error")
                                ),
                                true,
                            ),
                            (n, _) => (
                                format!(
                                    "Refreshed {}: {}. {} feed{} failed: {}",
                                    name,
                                    new_stories,
                                    n,
                                    if n == 1 { "" } else { "s" },
                                    failed.join(", ")
                                ),
                                true,
                            ),
                        }
                    }
                    Err(e) => (format!("Couldn't refresh {}: {}", name, e), true),
                });
                Task::none()
            }
            Message::MarkAllRead => {
                let (target_feed, target_folder) = match self.active_filter {
                    ActiveFilter::Feed(fid) => (Some(fid), None),
                    ActiveFilter::Folder(foid) => (None, Some(foid)),
                    _ => (None, None),
                };
                let _ = self.db.set_all_articles_read(target_feed, target_folder);
                self.reload_data();
                Task::none()
            }
            Message::DeleteFeed(feed_id) => {
                self.feed_menu = None;
                if self.renaming_feed.as_ref().is_some_and(|(id, _)| *id == feed_id) {
                    self.renaming_feed = None;
                }
                let title = self
                    .feeds
                    .iter()
                    .find(|f| f.id == feed_id)
                    .map(|f| f.title.clone())
                    .unwrap_or_else(|| "Feed".to_string());
                match self.db.delete_feed(feed_id) {
                    Ok(()) => {
                        self.status_message = Some((format!("Deleted '{}'", title), false));
                    }
                    Err(e) => {
                        self.status_message =
                            Some((format!("Failed to delete '{}': {}", title, e), true));
                    }
                }
                if self.active_filter == ActiveFilter::Feed(feed_id) {
                    self.active_filter = ActiveFilter::All;
                }
                self.reload_data();
                Task::none()
            }
            Message::OpenFeedMenu(feed_id) => {
                self.folder_menu = None;
                self.article_menu = None;
                self.reader_text_menu = None;
                self.feed_menu = Some(FeedMenu { feed_id, page: FeedMenuPage::Main });
                Task::none()
            }
            Message::CloseFeedMenu => {
                self.feed_menu = None;
                Task::none()
            }
            Message::AskDeleteFeed(feed_id) => {
                self.feed_menu = Some(FeedMenu { feed_id, page: FeedMenuPage::ConfirmDelete });
                Task::none()
            }
            Message::StartRenameFeed(feed_id) => {
                self.feed_menu = None;
                let title = self
                    .feeds
                    .iter()
                    .find(|f| f.id == feed_id)
                    .map(|f| f.title.clone())
                    .unwrap_or_default();
                self.renaming_folder = None;
                self.renaming_feed = Some((feed_id, title));
                let id = text_input::Id::new(RENAME_FEED_INPUT_ID);
                Task::batch([text_input::focus(id.clone()), text_input::select_all(id)])
            }
            Message::RenameFeedInputChanged(value) => {
                if let Some((_, title)) = &mut self.renaming_feed {
                    *title = value;
                }
                Task::none()
            }
            Message::SubmitRenameFeed => {
                if let Some((feed_id, title)) = self.renaming_feed.take() {
                    let title = title.trim();
                    // An empty name would also let the next sync replace it with the
                    // feed's own title, so treat it as "cancel"
                    if !title.is_empty() {
                        if let Err(e) = self.db.rename_feed(feed_id, title) {
                            self.status_message = Some((format!("Failed to rename feed: {}", e), true));
                        }
                        self.reload_data();
                    }
                }
                Task::none()
            }
            Message::ShowMoveToMenu(feed_id) => {
                self.feed_menu = Some(FeedMenu { feed_id, page: FeedMenuPage::MoveTo });
                Task::none()
            }
            Message::FeedMenuBack(feed_id) => {
                self.feed_menu = Some(FeedMenu { feed_id, page: FeedMenuPage::Main });
                Task::none()
            }
            Message::MoveFeedTo(feed_id, folder_id) => {
                self.feed_menu = None;
                // Same path as dropping onto a folder header: goes to the end of
                // the folder, with a "Moved … into …" toast
                self.drop_feed(feed_id, DropTarget::Folder(folder_id));
                Task::none()
            }
            Message::OpenEditFeed(feed_id) => {
                self.feed_menu = None;
                self.renaming_feed = None;
                if let Some(feed) = self.feeds.iter().find(|f| f.id == feed_id) {
                    self.editing_feed = Some(EditFeedDialog {
                        feed_id,
                        title: feed.title.clone(),
                        url: feed.url.clone(),
                        error: None,
                    });
                    return text_input::focus(text_input::Id::new(EDIT_FEED_TITLE_INPUT_ID));
                }
                Task::none()
            }
            Message::EditFeedTitleChanged(value) => {
                if let Some(dialog) = &mut self.editing_feed {
                    dialog.title = value;
                    dialog.error = None;
                }
                Task::none()
            }
            Message::EditFeedUrlChanged(value) => {
                if let Some(dialog) = &mut self.editing_feed {
                    dialog.url = value;
                    dialog.error = None;
                }
                Task::none()
            }
            Message::SaveEditFeed => {
                let Some(dialog) = &mut self.editing_feed else {
                    return Task::none();
                };
                let title = dialog.title.trim().to_string();
                let url = dialog.url.trim().to_string();

                if title.is_empty() {
                    dialog.error = Some("Name can't be empty.".to_string());
                    return Task::none();
                }
                match url::Url::parse(&url) {
                    Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                    _ => {
                        dialog.error =
                            Some("Feed URL must be a full http:// or https:// address.".to_string());
                        return Task::none();
                    }
                }
                if self.feeds.iter().any(|f| f.url == url && f.id != dialog.feed_id) {
                    dialog.error = Some("You're already subscribed to that URL.".to_string());
                    return Task::none();
                }

                match self.db.update_feed_details(dialog.feed_id, &title, &url) {
                    Ok(()) => {
                        self.editing_feed = None;
                        self.reload_data();
                    }
                    Err(e) => dialog.error = Some(format!("Couldn't save: {}", e)),
                }
                Task::none()
            }
            Message::Noop => Task::none(),
            Message::CancelRenameFeed => {
                self.renaming_feed = None;
                Task::none()
            }
            Message::CancelEditFeed => {
                self.editing_feed = None;
                Task::none()
            }
            Message::EscapePressed => {
                // From a sub-page (Move to / Delete confirm), Esc goes back first
                if let Some(menu) = self.feed_menu {
                    self.feed_menu = match menu.page {
                        FeedMenuPage::Main => None,
                        _ => Some(FeedMenu { page: FeedMenuPage::Main, ..menu }),
                    };
                }
                if let Some(menu) = self.folder_menu {
                    self.folder_menu = menu
                        .confirm_delete
                        .then_some(FolderMenu { confirm_delete: false, ..menu });
                }
                Task::none()
            }
            Message::OpenUrl(url) => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn();
                Task::none()
            }
            Message::ShowMoreArticles => {
                self.article_render_limit += ARTICLE_PAGE_SIZE;
                Task::none()
            }
            Message::StatusTimedOut(generation) => {
                if generation == self.status_generation {
                    self.status_message = None;
                }
                Task::none()
            }
            Message::DismissStatus => {
                self.status_message = None;
                Task::none()
            }

            // Folder Handlers
            Message::ToggleFolderExpand(folder_id) => {
                if self.expanded_folders.contains(&folder_id) {
                    self.expanded_folders.remove(&folder_id);
                } else {
                    self.expanded_folders.insert(folder_id);
                }
                Task::none()
            }
            Message::ShowNewFolderInput => {
                self.show_new_folder_input = true;
                self.new_folder_input.clear();
                // Immediately focus the input box so the user can type without clicking!
                text_input::focus(text_input::Id::new(FOLDER_INPUT_ID))
            }
            Message::CancelNewFolderInput => {
                self.show_new_folder_input = false;
                self.new_folder_input.clear();
                Task::none()
            }
            Message::NewFolderInputChanged(val) => {
                self.new_folder_input = val;
                Task::none()
            }
            Message::CreateFolderClicked => {
                let name = self.new_folder_input.trim().to_string();
                if !name.is_empty() {
                    match self.db.create_folder(&name) {
                        Ok(id) => {
                            self.expanded_folders.insert(id);
                            self.new_folder_input.clear();
                            self.show_new_folder_input = false;
                            self.status_message = Some((format!("Created folder '{}'", name), false));
                            self.reload_data();
                        }
                        Err(e) => {
                            self.status_message = Some((format!("Failed to create folder: {}", e), true));
                        }
                    }
                }
                Task::none()
            }
            Message::DeleteFolder(folder_id) => {
                self.folder_menu = None;
                if self.renaming_folder.as_ref().is_some_and(|(id, _)| *id == folder_id) {
                    self.renaming_folder = None;
                }
                let (name, feed_count) = self
                    .folders
                    .iter()
                    .find(|f| f.id == folder_id)
                    .map(|f| (f.name.clone(), f.feed_count))
                    .unwrap_or_else(|| ("Folder".to_string(), 0));
                match self.db.delete_folder(folder_id) {
                    Ok(()) => {
                        let moved = match feed_count {
                            0 => String::new(),
                            1 => "; its feed moved to Uncategorized".to_string(),
                            n => format!("; its {} feeds moved to Uncategorized", n),
                        };
                        self.status_message =
                            Some((format!("Deleted folder '{}'{}", name, moved), false));
                    }
                    Err(e) => {
                        self.status_message =
                            Some((format!("Failed to delete folder '{}': {}", name, e), true));
                    }
                }
                self.expanded_folders.remove(&folder_id);
                if self.active_filter == ActiveFilter::Folder(folder_id) {
                    self.active_filter = ActiveFilter::All;
                }
                self.reload_data();
                Task::none()
            }
            Message::OpenFolderMenu(folder_id) => {
                self.feed_menu = None;
                self.article_menu = None;
                self.reader_text_menu = None;
                self.folder_menu = Some(FolderMenu { folder_id, confirm_delete: false });
                Task::none()
            }
            Message::CloseFolderMenu => {
                self.folder_menu = None;
                Task::none()
            }
            Message::AskDeleteFolder(folder_id) => {
                self.folder_menu = Some(FolderMenu { folder_id, confirm_delete: true });
                Task::none()
            }
            Message::StartRenameFolder(folder_id) => {
                self.folder_menu = None;
                self.renaming_feed = None;
                let name = self
                    .folders
                    .iter()
                    .find(|f| f.id == folder_id)
                    .map(|f| f.name.clone())
                    .unwrap_or_default();
                self.renaming_folder = Some((folder_id, name));
                let id = text_input::Id::new(RENAME_FOLDER_INPUT_ID);
                Task::batch([text_input::focus(id.clone()), text_input::select_all(id)])
            }
            Message::RenameFolderInputChanged(value) => {
                if let Some((_, name)) = &mut self.renaming_folder {
                    *name = value;
                }
                Task::none()
            }
            Message::SubmitRenameFolder => {
                let Some((folder_id, name)) = &self.renaming_folder else {
                    return Task::none();
                };
                let (folder_id, name) = (*folder_id, name.trim().to_string());
                // Empty name = cancel, same as feed rename
                if name.is_empty() {
                    self.renaming_folder = None;
                    return Task::none();
                }
                // Folder names are unique. Keep the field open so the name can be fixed.
                if self
                    .folders
                    .iter()
                    .any(|f| f.id != folder_id && f.name.eq_ignore_ascii_case(&name))
                {
                    self.status_message =
                        Some((format!("A folder named '{}' already exists", name), true));
                    return Task::none();
                }
                self.renaming_folder = None;
                if let Err(e) = self.db.rename_folder(folder_id, &name) {
                    self.status_message = Some((format!("Failed to rename folder: {}", e), true));
                }
                self.reload_data();
                Task::none()
            }
            Message::CancelRenameFolder => {
                self.renaming_folder = None;
                Task::none()
            }

            // Drag-and-Drop Handlers
            Message::DragFeedStarted(feed_id) => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.dragging_feed = Some(feed_id);
                self.drag_hover_target = None;
                // No status banner here: showing one would push the whole layout down
                // under the cursor. The drag hint lives in the floating badge instead.
                Task::none()
            }
            Message::DragOverTarget(target) => {
                if self.dragging_feed.is_some() {
                    self.drag_hover_target = Some(target);
                }
                Task::none()
            }
            Message::DragLeaveTarget(target) => {
                if self.drag_hover_target == Some(target) {
                    self.drag_hover_target = None;
                }
                Task::none()
            }
            // Both release paths end up here. Whichever arrives first performs the
            // drop; the other then finds no drag in progress and does nothing.
            Message::DropOnTarget(target) => {
                self.finish_drag(Some(target));
                Task::none()
            }
            Message::DragFeedDropped => {
                self.finish_drag(self.drag_hover_target);
                Task::none()
            }
        }
    }

    fn finish_drag(&mut self, target: Option<DropTarget>) {
        if let (Some(feed_id), Some(target)) = (self.dragging_feed, target) {
            self.drop_feed(feed_id, target);
        }
        self.dragging_feed = None;
        self.drag_hover_target = None;
    }

    /// Adds a feed and fetches its stories in the background (reports via
    /// `FeedAdded`). With no `title`, the URL stands in until the first sync
    /// replaces it with the feed's own title.
    fn subscribe(&mut self, url: String, title: Option<String>, folder_id: Option<i64>) -> Task<Message> {
        self.is_syncing = true;
        let db = self.db.clone();
        let fetcher = self.fetcher.clone();

        Task::perform(
            async move {
                let title = title.unwrap_or_else(|| url.clone());
                let feed_id = db
                    .add_or_update_feed(&url, &title, None, None, folder_id)
                    .map_err(|e| format!("Database error: {}", e))?;

                let summary = fetcher
                    .fetch_and_sync(&db, feed_id)
                    .await
                    .map_err(|e| format!("Feed fetch error: {}", e))?;
                if let Some(error) = summary.error {
                    return Err(format!("Added '{}', but fetching it failed: {}", title, error));
                }
                Ok((feed_id, url))
            },
            Message::FeedAdded,
        )
    }

    /// Fetches the given feeds in the background; reports with
    /// `PartialRefreshFinished`. Shares the "syncing" state with Refresh All,
    /// so the two never run at once.
    fn refresh_feeds(&mut self, name: String, feed_ids: Vec<i64>) -> Task<Message> {
        if self.is_syncing {
            self.status_message =
                Some(("A refresh is already running. Try again when it finishes.".to_string(), true));
            return Task::none();
        }
        if feed_ids.is_empty() {
            return Task::none();
        }
        self.is_syncing = true;
        let db = self.db.clone();
        let fetcher = self.fetcher.clone();
        Task::perform(
            async move {
                let mut summaries = Vec::with_capacity(feed_ids.len());
                for feed_id in feed_ids {
                    summaries.push(
                        fetcher
                            .fetch_and_sync(&db, feed_id)
                            .await
                            .map_err(|e| e.to_string())?,
                    );
                }
                Ok(summaries)
            },
            move |result| Message::PartialRefreshFinished(name.clone(), result),
        )
    }

    /// Marks a story read when it's opened. Updates the counts in memory
    /// instead of reloading everything from the database, which took ~100 ms
    /// per click with a few thousand stories.
    fn mark_opened_story_read(&mut self, article_id: i64) {
        let Some(article) = self.articles.iter_mut().find(|a| a.id == article_id) else {
            return;
        };
        if article.is_read {
            return;
        }
        if let Err(e) = self.db.set_article_read(article_id, true) {
            self.status_message = Some((format!("Failed to mark story read: {}", e), true));
            return;
        }
        article.is_read = true;
        let feed_id = article.feed_id;

        self.stats.2 = (self.stats.2 - 1).max(0);
        if let Some(feed) = self.feeds.iter_mut().find(|f| f.id == feed_id) {
            feed.unread_count = (feed.unread_count - 1).max(0);
            if let Some(folder_id) = feed.folder_id {
                if let Some(folder) = self.folders.iter_mut().find(|f| f.id == folder_id) {
                    folder.unread_count = (folder.unread_count - 1).max(0);
                }
            }
        }
    }

    /// Forgets images that aren't in `blocks` (the newly opened story), and
    /// failed ones so they're retried. Downloads start as images scroll into
    /// view (`LoadStoryImage`), not all at once: a story can have 100+ large
    /// images, ~1.5 GB once decoded.
    fn prune_story_images(&mut self, blocks: &[Block]) {
        let urls: HashSet<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Image { src } => Some(src.as_str()),
                Block::Text(_) => None,
            })
            .collect();
        // Keep what this story still uses (e.g. reopening the same story); retry failures
        self.story_images
            .retain(|url, state| urls.contains(url.as_str()) && !matches!(state, ImageState::Failed));
    }

    /// The story body: text paragraphs with the story's images in between.
    ///
    /// With `selectable`, text blocks are the read-only editors in
    /// `reader_texts` (one per text block, in order) so they can be copied.
    fn story_body<'a>(&'a self, blocks: &[Block], selectable: bool) -> Element<'a, Message> {
        let mut body = column![].spacing(14).width(Length::Fill);
        let mut text_index = 0;
        for block in blocks {
            body = body.push(match block {
                Block::Text(text_block) => {
                    let index = text_index;
                    text_index += 1;
                    match self.reader_texts.get(index).filter(|_| selectable) {
                        Some(content) => self.selectable_text(index, content),
                        None => text(text_block.clone()).size(14).width(Length::Fill).into(),
                    }
                }
                Block::Image { src } => self.story_image(src),
            });
        }
        body.into()
    }

    /// A story text block that can be selected (drag, double/triple click,
    /// Ctrl+A) and copied (Ctrl+C or right click). Styled to look like plain text.
    fn selectable_text<'a>(
        &'a self,
        index: usize,
        content: &'a text_editor::Content,
    ) -> Element<'a, Message> {
        let editor = text_editor(content)
            .on_action(move |action| Message::ReaderTextAction(index, action))
            .size(14)
            .padding(0)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .style(|theme: &Theme, _status| {
                let palette = theme.extended_palette();
                text_editor::Style {
                    background: iced::Color::TRANSPARENT.into(),
                    border: iced::Border::default(),
                    icon: palette.background.weak.text,
                    placeholder: palette.background.strong.color,
                    value: palette.background.base.text,
                    selection: palette.primary.weak.color,
                }
            });

        let has_selection = content.selection().is_some();
        let menu = (self.reader_text_menu == Some(index)).then(|| {
            let menu_item = |label: &'static str, message: Option<Message>| {
                button(text(label).size(13))
                    .width(Length::Fill)
                    .padding([5, 10])
                    .style(button::text)
                    .on_press_maybe(message)
            };
            menu_card(
                column![
                    menu_item("Copy", has_selection.then_some(Message::CopyReaderSelection(index))),
                    menu_item("Select all", Some(Message::SelectAllReaderText(index))),
                ]
                .spacing(2),
                150.0,
            )
        });

        context_menu(editor)
            .menu(menu)
            .focus_on_right_press()
            .on_right_press(Message::OpenReaderTextMenu(index))
            .on_dismiss(Message::CloseReaderTextMenu)
            .into()
    }

    /// One story image, centered. Clicking it opens the full image in the browser.
    fn story_image<'a>(&'a self, src: &str) -> Element<'a, Message> {
        let open = Message::OpenUrl(src.to_string());
        let content: Element<'a, Message> = match self.story_images.get(src) {
            // Shrink-sized: small images stay at their own size, large ones are
            // scaled down to the column width (keeping their proportions)
            Some(ImageState::Raster(handle)) => mouse_area(iced::widget::image(handle.clone()))
                .on_press(open)
                .interaction(mouse::Interaction::Pointer)
                .into(),
            Some(ImageState::Svg(handle)) => mouse_area(
                iced::widget::svg(handle.clone())
                    .width(Length::Shrink)
                    .height(Length::Shrink),
            )
            .on_press(open)
            .interaction(mouse::Interaction::Pointer)
            .into(),
            Some(ImageState::Loading) => {
                text("Loading image…").size(11).style(text::secondary).into()
            }
            // Not requested yet: ask for it once it's near the visible area
            None => on_visible(
                text("Loading image…").size(11).style(text::secondary),
                Message::LoadStoryImage(src.to_string()),
                IMAGE_PRELOAD_MARGIN,
            )
            .into(),
            Some(ImageState::Failed) => button(
                text("Image couldn't be loaded. Open it in the browser").size(11),
            )
            .padding([2, 6])
            .style(button::text)
            .on_press(open)
            .into(),
        };
        container(content).center_x(Length::Fill).into()
    }

    /// The folder a new feed goes into: the selected folder, or the folder of
    /// the selected feed.
    fn default_new_feed_folder(&self) -> Option<i64> {
        match self.active_filter {
            ActiveFilter::Folder(fid) => Some(fid),
            ActiveFilter::Feed(fid) => self.feeds.iter().find(|f| f.id == fid).and_then(|f| f.folder_id),
            _ => None,
        }
    }

    fn folder_choices(&self) -> Vec<FolderChoice> {
        std::iter::once(uncategorized_choice())
            .chain(self.folders.iter().map(|f| FolderChoice { id: Some(f.id), name: f.name.clone() }))
            .collect()
    }

    /// Feed ids in `folder_id` (None = Uncategorized) in display order.
    fn feed_ids_in(&self, folder_id: Option<i64>) -> Vec<i64> {
        self.feeds
            .iter()
            .filter(|f| f.folder_id == folder_id)
            .map(|f| f.id)
            .collect()
    }

    fn folder_label(&self, folder_id: Option<i64>) -> String {
        match folder_id {
            Some(id) => self
                .folders
                .iter()
                .find(|f| f.id == id)
                .map(|f| format!("folder '{}'", f.name))
                .unwrap_or_else(|| "folder".to_string()),
            None => "Uncategorized".to_string(),
        }
    }

    fn drop_feed(&mut self, feed_id: i64, target: DropTarget) {
        let Some(dragged) = self.feeds.iter().find(|f| f.id == feed_id) else {
            return;
        };
        let source_folder = dragged.folder_id;
        let feed_title = dragged.title.clone();

        let (dest_folder, result) = match target {
            DropTarget::Folder(dest) => {
                if dest == source_folder {
                    return; // dropped back on its own folder: nothing to do
                }
                (dest, self.db.set_feed_folder(feed_id, dest))
            }
            DropTarget::Feed(target_id) => {
                if target_id == feed_id {
                    return;
                }
                let Some(dest) = self.feeds.iter().find(|f| f.id == target_id).map(|f| f.folder_id)
                else {
                    return;
                };
                let original = self.feed_ids_in(dest);
                let Some(target_idx) = original.iter().position(|&id| id == target_id) else {
                    return;
                };
                let mut order: Vec<i64> = original.iter().copied().filter(|&id| id != feed_id).collect();
                // Inserting at the target's original index makes the dragged feed take
                // the target's place: it lands after the target when moving down
                // within a folder, and before it when moving up or coming from elsewhere.
                order.insert(target_idx.min(order.len()), feed_id);
                (dest, self.db.place_feeds(dest, &order))
            }
        };

        match result {
            Ok(()) => {
                // Reordering within a folder is self-evident; only announce moves
                if dest_folder != source_folder {
                    self.status_message = Some((
                        format!("Moved '{}' into {}", feed_title, self.folder_label(dest_folder)),
                        false,
                    ));
                }
            }
            Err(e) => {
                self.status_message = Some((format!("Failed to move '{}': {}", feed_title, e), true));
            }
        }
        self.reload_data();
    }

    /// Where to draw the insertion line on `feed`'s row, if a drag is hovering it.
    fn insert_edge_for(&self, feed: &Feed) -> Option<InsertEdge> {
        let dragged_id = self.dragging_feed?;
        if dragged_id == feed.id || self.drag_hover_target != Some(DropTarget::Feed(feed.id)) {
            return None;
        }
        let dragged = self.feeds.iter().find(|f| f.id == dragged_id)?;
        if dragged.folder_id == feed.folder_id {
            let ids = self.feed_ids_in(feed.folder_id);
            let from = ids.iter().position(|&id| id == dragged_id)?;
            let to = ids.iter().position(|&id| id == feed.id)?;
            if from < to {
                return Some(InsertEdge::Bottom);
            }
        }
        Some(InsertEdge::Top)
    }

    /// One feed row in the sidebar tree. While a drag is in progress, every row
    /// other than the dragged one is a drop target for reordering.
    fn feed_row<'a>(&'a self, feed: &'a Feed, indented: bool) -> Element<'a, Message> {
        let is_dragging = self.dragging_feed.is_some();
        let is_feed_active = self.active_filter == ActiveFilter::Feed(feed.id);
        let is_this_feed_dragged = self.dragging_feed == Some(feed.id);

        let feed_unread = if feed.unread_count > 0 {
            format!("{}", feed.unread_count)
        } else {
            String::new()
        };

        // Drag grip handle
        let grip_btn = mouse_area(
            container(grip_icon(is_this_feed_dragged))
                .padding([4, 6])
                .style(if is_this_feed_dragged {
                    container::bordered_box
                } else {
                    container::transparent
                }),
        )
        .on_press(Message::DragFeedStarted(feed.id))
        .interaction(if is_this_feed_dragged {
            mouse::Interaction::Grabbing
        } else {
            mouse::Interaction::Grab
        });

        let renaming_title = self
            .renaming_feed
            .as_ref()
            .filter(|(id, _)| *id == feed.id)
            .map(|(_, title)| title);

        let feed_btn: Element<'a, Message> = if let Some(title) = renaming_title {
            // In-place rename: Enter saves, Esc cancels
            // on_escape: the focused input would otherwise swallow Esc
            on_escape(
                text_input("Feed name", title)
                    .id(text_input::Id::new(RENAME_FEED_INPUT_ID))
                    .on_input(Message::RenameFeedInputChanged)
                    .on_submit(Message::SubmitRenameFeed)
                    .size(12)
                    .padding([2, 6])
                    .width(Length::Fill),
                Message::CancelRenameFeed,
            )
            .into()
        } else {
            button(
            row![
                text(&feed.title).size(12).width(Length::Fill),
                if !feed_unread.is_empty() {
                    text(feed_unread).size(11).style(text::primary)
                } else {
                    text("")
                },
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding([3, 6])
        .style(if is_this_feed_dragged {
            button::secondary
        } else if is_feed_active {
            button::primary
        } else {
            button::text
        })
        .on_press(Message::SelectFilter(ActiveFilter::Feed(feed.id)))
        .into()
        };

        let feed_row = row![
            Space::with_width(if indented { 12 } else { 0 }),
            Element::from(grip_btn),
            feed_btn,
        ]
        .spacing(2)
        .align_y(Alignment::Center);

        // Insertion line drawn as an overlay layer: `stack` takes its size from the
        // row alone, so showing the line never shifts anything.
        let overlay: Element<'a, Message> = match self.insert_edge_for(feed) {
            Some(edge) => container(
                container(Space::new(Length::Fill, Length::Fixed(2.0))).style(|theme: &Theme| {
                    container::Style {
                        background: Some(theme.extended_palette().primary.strong.color.into()),
                        ..Default::default()
                    }
                }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(iced::Padding::ZERO.left(if indented { 12 } else { 0 }))
            .align_y(match edge {
                InsertEdge::Top => iced::alignment::Vertical::Top,
                InsertEdge::Bottom => iced::alignment::Vertical::Bottom,
            })
            .into(),
            None => Space::new(Length::Shrink, Length::Shrink).into(),
        };

        let mut drop_area = mouse_area(stack![feed_row, overlay].width(Length::Fill));
        if is_dragging && !is_this_feed_dragged {
            let target = DropTarget::Feed(feed.id);
            drop_area = drop_area
                .on_enter(Message::DragOverTarget(target))
                .on_exit(Message::DragLeaveTarget(target))
                .on_release(Message::DropOnTarget(target))
                .interaction(mouse::Interaction::Grabbing);
        }

        let menu = self
            .feed_menu
            .filter(|m| m.feed_id == feed.id && !is_dragging)
            .map(|m| self.feed_menu_view(feed, m.page));

        let mut row_with_menu = context_menu(drop_area)
            .menu(menu)
            .on_dismiss(Message::CloseFeedMenu)
            .on_escape(Message::EscapePressed);
        if !is_dragging {
            row_with_menu = row_with_menu.on_right_press(Message::OpenFeedMenu(feed.id));
        }
        row_with_menu.into()
    }

    /// Modal "Edit feed" dialog, drawn over a dimmed backdrop.
    fn edit_feed_dialog_view<'a>(&'a self, dialog: &'a EditFeedDialog) -> Element<'a, Message> {
        let url_changed = self
            .feeds
            .iter()
            .find(|f| f.id == dialog.feed_id)
            .is_some_and(|f| f.url != dialog.url.trim());

        let mut form = column![
            text("Edit feed").size(16),
            column![
                text("Name").size(12).style(text::secondary),
                text_input("Feed name", &dialog.title)
                    .id(text_input::Id::new(EDIT_FEED_TITLE_INPUT_ID))
                    .on_input(Message::EditFeedTitleChanged)
                    .on_submit(Message::SaveEditFeed)
                    .size(13)
                    .padding(6),
            ]
            .spacing(4),
            column![
                text("Feed URL").size(12).style(text::secondary),
                text_input("https://example.com/feed.xml", &dialog.url)
                    .on_input(Message::EditFeedUrlChanged)
                    .on_submit(Message::SaveEditFeed)
                    .size(13)
                    .padding(6),
            ]
            .spacing(4),
        ]
        .spacing(12);

        // One message line with a fixed height (always present, possibly empty), so
        // errors or the URL note never push the buttons around.
        let message: Element<'a, Message> = if let Some(error) = &dialog.error {
            text(error).size(12).style(text::danger).into()
        } else if url_changed {
            text("Archived stories are kept. The new URL is fetched on the next refresh.")
                .size(11)
                .style(text::secondary)
                .into()
        } else {
            Space::with_height(0).into()
        };
        form = form.push(container(message).height(Length::Fixed(30.0)));

        form = form.push(
            row![
                horizontal_space(),
                button(text("Cancel").size(13))
                    .padding([5, 12])
                    .style(button::text)
                    .on_press(Message::CancelEditFeed),
                button(text("Save").size(13))
                    .padding([5, 14])
                    .style(button::primary)
                    .on_press(Message::SaveEditFeed),
            ]
            .spacing(8),
        );

        modal(form, Message::CancelEditFeed)
    }

    /// Modal "Add feed" dialog. The name fills in from the feed once the URL
    /// has been looked up, and stays editable.
    fn add_feed_dialog_view<'a>(&'a self, dialog: &'a AddFeedDialog) -> Element<'a, Message> {
        let lookup_note: Element<'a, Message> = match &dialog.lookup {
            FeedLookup::Idle => Space::with_height(0).into(),
            FeedLookup::Pending => text("Looking up feed…").size(11).style(text::secondary).into(),
            FeedLookup::Found { story_count } => text(match story_count {
                0 => "Found the feed. It has no stories right now.".to_string(),
                1 => "Found the feed (1 story).".to_string(),
                n => format!("Found the feed ({} stories).", n),
            })
            .size(11)
            .style(text::success)
            .into(),
            FeedLookup::Failed(reason) => text(format!("{} You can still add it.", reason))
                .size(11)
                .style(text::danger)
                .into(),
        };

        let error: Element<'a, Message> = match &dialog.error {
            Some(error) => text(error).size(12).style(text::danger).into(),
            None => Space::with_height(0).into(),
        };

        let form = column![
            text("Add feed").size(16),
            column![
                text("Feed URL").size(12).style(text::secondary),
                text_input("https://example.com/feed.xml", &dialog.url)
                    .id(text_input::Id::new(ADD_FEED_URL_INPUT_ID))
                    .on_input(Message::AddFeedUrlChanged)
                    .on_submit(Message::SubmitAddFeed)
                    .size(13)
                    .padding(6),
                // Fixed height so the note appearing doesn't move the fields below
                container(lookup_note).height(Length::Fixed(16.0)),
            ]
            .spacing(4),
            column![
                text("Name").size(12).style(text::secondary),
                text_input(
                    if dialog.lookup == FeedLookup::Pending {
                        "Filling in from the feed…"
                    } else {
                        "Filled in from the feed"
                    },
                    &dialog.name,
                )
                .on_input(Message::AddFeedNameChanged)
                .on_submit(Message::SubmitAddFeed)
                .size(13)
                .padding(6),
            ]
            .spacing(4),
            column![
                text("Folder").size(12).style(text::secondary),
                pick_list(
                    self.folder_choices(),
                    Some(dialog.folder.clone()),
                    Message::AddFeedFolderSelected,
                )
                .text_size(13)
                .padding(6)
                .width(Length::Fill),
            ]
            .spacing(4),
            container(error).height(Length::Fixed(20.0)),
            row![
                horizontal_space(),
                button(text("Cancel").size(13))
                    .padding([5, 12])
                    .style(button::text)
                    .on_press(Message::CancelAddFeed),
                button(text("Add feed").size(13))
                    .padding([5, 14])
                    .style(button::primary)
                    .on_press_maybe(
                        (!dialog.url.trim().is_empty()).then_some(Message::SubmitAddFeed),
                    ),
            ]
            .spacing(8),
        ]
        .spacing(12);

        modal(form, Message::CancelAddFeed)
    }

    /// Contents of a feed's right-click menu.
    fn feed_menu_view<'a>(&'a self, feed: &'a Feed, page: FeedMenuPage) -> Element<'a, Message> {
        let menu_item = |label: String, message: Option<Message>| {
            button(text(label).size(13))
                .width(Length::Fill)
                .padding([5, 10])
                .style(button::text)
                .on_press_maybe(message)
        };

        let body: Element<'a, Message> = if page == FeedMenuPage::MoveTo {
            // Destination list; the feed's current location is labelled and disabled
            let destination = |label: String, folder_id: Option<i64>| {
                let is_current = feed.folder_id == folder_id;
                menu_item(
                    // (No ✓ glyph: the default font doesn't have it)
                    if is_current { format!("{} (current)", label) } else { label },
                    (!is_current).then_some(Message::MoveFeedTo(feed.id, folder_id)),
                )
            };
            let mut destinations = column![destination("Uncategorized".to_string(), None)];
            for folder in &self.folders {
                destinations = destinations.push(destination(folder.name.clone(), Some(folder.id)));
            }
            column![
                menu_item("← Back".to_string(), Some(Message::FeedMenuBack(feed.id))),
                horizontal_rule(1),
                container(scrollable(destinations)).max_height(320),
            ]
            .spacing(2)
            .into()
        } else if page == FeedMenuPage::ConfirmDelete {
            column![
                text(format!("Delete '{}'?", feed.title)).size(13),
                text(match feed.total_count {
                    0 => "It has no archived stories.".to_string(),
                    1 => "Its 1 archived story will be permanently deleted too.".to_string(),
                    n => format!("Its {} archived stories will be permanently deleted too.", n),
                })
                .size(11)
                .style(text::secondary),
                row![
                    horizontal_space(),
                    button(text("Cancel").size(12))
                        .padding([4, 10])
                        .style(button::text)
                        .on_press(Message::CloseFeedMenu),
                    button(text("Delete").size(12))
                        .padding([4, 10])
                        .style(button::danger)
                        .on_press(Message::DeleteFeed(feed.id)),
                ]
                .spacing(6),
            ]
            .spacing(8)
            .padding(8)
            .into()
        } else {
            column![
                menu_item(
                    "Refresh".to_string(),
                    (!self.is_syncing).then_some(Message::RefreshFeed(feed.id)),
                ),
                menu_item(
                    "Mark as read".to_string(),
                    (feed.unread_count > 0).then_some(Message::SetFeedRead(feed.id, true)),
                ),
                menu_item(
                    "Mark as unread".to_string(),
                    (feed.unread_count < feed.total_count)
                        .then_some(Message::SetFeedRead(feed.id, false)),
                ),
                horizontal_rule(1),
                menu_item("Rename".to_string(), Some(Message::StartRenameFeed(feed.id))),
                menu_item("Edit feed…".to_string(), Some(Message::OpenEditFeed(feed.id))),
                menu_item("Move to…".to_string(), Some(Message::ShowMoveToMenu(feed.id))),
                horizontal_rule(1),
                menu_item("Delete…".to_string(), Some(Message::AskDeleteFeed(feed.id))),
            ]
            .spacing(2)
            .into()
        };

        menu_card(
            body,
            match page {
                FeedMenuPage::Main => 160.0,
                FeedMenuPage::MoveTo => 220.0,
                FeedMenuPage::ConfirmDelete => 260.0,
            },
        )
    }

    /// Contents of a folder's right-click menu.
    fn folder_menu_view<'a>(&'a self, folder: &'a Folder, confirm_delete: bool) -> Element<'a, Message> {
        if confirm_delete {
            let body = column![
                text(format!("Delete folder '{}'?", folder.name)).size(13),
                text(match folder.feed_count {
                    0 => "The folder is empty.".to_string(),
                    1 => "Its feed will move to Uncategorized. No feeds or stories are deleted."
                        .to_string(),
                    n => format!(
                        "Its {} feeds will move to Uncategorized. No feeds or stories are deleted.",
                        n
                    ),
                })
                .size(11)
                .style(text::secondary),
                row![
                    horizontal_space(),
                    button(text("Cancel").size(12))
                        .padding([4, 10])
                        .style(button::text)
                        .on_press(Message::CloseFolderMenu),
                    button(text("Delete").size(12))
                        .padding([4, 10])
                        .style(button::danger)
                        .on_press(Message::DeleteFolder(folder.id)),
                ]
                .spacing(6),
            ]
            .spacing(8)
            .padding(8);
            return menu_card(body, 260.0);
        }

        // A `None` message shows the item disabled
        let menu_item = |label: &'static str, message: Option<Message>| {
            button(text(label).size(13))
                .width(Length::Fill)
                .padding([5, 10])
                .style(button::text)
                .on_press_maybe(message)
        };
        let body = column![
            menu_item(
                "Refresh",
                (!self.is_syncing && folder.feed_count > 0)
                    .then_some(Message::RefreshFolder(folder.id)),
            ),
            menu_item(
                "Mark as read",
                (folder.unread_count > 0).then_some(Message::SetFolderRead(folder.id, true)),
            ),
            menu_item(
                "Mark as unread",
                (folder.unread_count < folder.total_count)
                    .then_some(Message::SetFolderRead(folder.id, false)),
            ),
            horizontal_rule(1),
            menu_item("Rename", Some(Message::StartRenameFolder(folder.id))),
            horizontal_rule(1),
            menu_item("Delete…", Some(Message::AskDeleteFolder(folder.id))),
        ]
        .spacing(2);
        menu_card(body, 160.0)
    }

    pub fn view(&self) -> Element<'_, Message> {
        let sidebar = self.view_sidebar();
        let article_list = self.view_article_list();
        let reader_view = self.view_reader();

        let content = row![
            sidebar,
            vertical_rule(1),
            split(article_list, reader_view, self.article_list_width, Message::ArticleListResized)
                .min_widths(ARTICLE_LIST_MIN_WIDTH, READER_MIN_WIDTH)
                .on_resize_end(Message::ArticleListResizeEnded),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        let mut layers: Vec<Element<'_, Message>> = vec![content.into()];

        // Status messages float as a toast over the bottom-right corner instead of
        // being a banner above the content: a banner pushes the whole layout down
        // whenever it appears or is dismissed (e.g. a sync finishing mid-drag).
        if let Some((ref msg, is_error)) = self.status_message {
            let toast = container(
                row![
                    text(msg.clone())
                        .size(13)
                        .width(Length::Fill)
                        .style(if is_error { text::danger } else { text::default }),
                    button(text("Dismiss").size(12))
                        .padding([2, 8])
                        .on_press(Message::DismissStatus),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([8, 12])
            .max_width(460)
            .style(container::bordered_box);

            layers.push(
                container(toast)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Right)
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(12)
                    .into(),
            );
        }

        // If dragging, render floating preview badge that follows the mouse cursor!
        if let Some(dialog) = &self.editing_feed {
            layers.push(self.edit_feed_dialog_view(dialog));
        }
        if let Some(dialog) = &self.add_feed {
            layers.push(self.add_feed_dialog_view(dialog));
        }

        if let Some(feed_id) = self.dragging_feed {
            let feed_title = self
                .feeds
                .iter()
                .find(|f| f.id == feed_id)
                .map(|f| f.title.as_str())
                .unwrap_or("Feed");

            let floating_badge = container(
                row![
                    grip_icon(true),
                    column![
                        text(format!("Dragging: {}", feed_title)).size(13).style(text::primary),
                        text("Drop on a folder to move, or on a feed to reorder")
                            .size(11)
                            .style(text::secondary),
                    ]
                    .spacing(2),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([8, 14])
            .style(container::bordered_box);

            // Offset from the cursor so the badge doesn't cover the drop target being hovered
            let floating_layer: Element<'_, Message> =
                follow_cursor(floating_badge, Vector::new(15.0, -12.0))
                    .on_release(Message::DragFeedDropped)
                    .into();

            layers.push(floating_layer);
        }

        stack(layers)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_sidebar(&self) -> Element<'_, Message> {
        // App Header
        let title_bar = column![
            text("RSS Archive Reader").size(18),
            text("Permanent SQL Archive")
                .size(11)
                .style(text::secondary),
        ]
        .spacing(2);

        // Refresh Button
        let refresh_label: Element<'_, Message> = if self.is_syncing {
            row![spinner(Icon::Refresh, 13.0, Tint::OnPrimary), text("Syncing Feeds...").size(13)]
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
        } else {
            row![icon(Icon::Refresh, 13.0, Tint::OnPrimary), text("Refresh All Feeds").size(13)]
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
        };
        let refresh_btn = button(refresh_label)
            .width(Length::Fill)
            .padding([6, 10])
            .on_press(Message::RefreshClicked);

        // Core Category Filters
        let (total_feeds, total_articles, unread_articles) = self.stats;

        let all_btn = self.filter_button(
            ActiveFilter::All,
            None,
            "All Stories",
            &total_articles.to_string(),
        );

        let unread_btn = self.filter_button(
            ActiveFilter::Unread,
            None,
            "Unread",
            &unread_articles.to_string(),
        );

        let starred_count = self
            .articles
            .iter()
            .filter(|a| a.is_starred)
            .count();
        let starred_btn = self.filter_button(
            ActiveFilter::Starred,
            Some(Icon::StarFilled),
            "Starred",
            &starred_count.to_string(),
        );

        let categories = column![all_btn, unread_btn, starred_btn].spacing(4);

        // Subscriptions & Folders Header
        let subscriptions_header = row![
            text("SUBSCRIPTIONS").size(11).style(text::secondary),
            horizontal_space(),
            button(text("+ Feed").size(11))
                .padding([2, 6])
                .style(button::secondary)
                .on_press(Message::OpenAddFeed),
            button(text("+ Folder").size(11))
                .padding([2, 6])
                .style(button::secondary)
                .on_press(Message::ShowNewFolderInput),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        // New Folder Input Row (Immediately focused on creation!)
        let mut tree_items = column![].spacing(4);

        if self.show_new_folder_input {
            let new_folder_box = container(
                row![
                    text_input("Folder name...", &self.new_folder_input)
                        .id(text_input::Id::new(FOLDER_INPUT_ID))
                        .on_input(Message::NewFolderInputChanged)
                        .on_submit(Message::CreateFolderClicked)
                        .padding(5)
                        .size(12),
                    button(text("Create").size(11))
                        .padding([4, 8])
                        .style(button::primary)
                        .on_press(Message::CreateFolderClicked),
                    button(text("×").size(12))
                        .padding([4, 6])
                        .style(button::text)
                        .on_press(Message::CancelNewFolderInput),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            )
            .padding(4);

            tree_items = tree_items.push(new_folder_box);
        }


        let is_dragging = self.dragging_feed.is_some();

        // 1. Folders List with nested feeds (Akregator style + Drag & Drop Target)
        for folder in &self.folders {
            let is_expanded = self.expanded_folders.contains(&folder.id);
            let is_folder_active = self.active_filter == ActiveFilter::Folder(folder.id);
            let is_drop_hovered = self.drag_hover_target == Some(DropTarget::Folder(Some(folder.id)));

            let chevron = if is_expanded { "▼" } else { "▶" };

            let unread_badge = if folder.unread_count > 0 {
                format!("{}", folder.unread_count)
            } else {
                String::new()
            };

            {
                // Folder header row. While dragging, the same row doubles as the drop
                // target and is only restyled, never resized, so nothing below it
                // shifts under the cursor.
                let toggle_btn = button(text(chevron).size(10))
                    .padding([2, 4])
                    .style(button::text)
                    .on_press(Message::ToggleFolderExpand(folder.id));

                let renaming_name = self
                    .renaming_folder
                    .as_ref()
                    .filter(|(id, _)| *id == folder.id)
                    .map(|(_, name)| name);

                let folder_btn: Element<'_, Message> = if let Some(name) = renaming_name {
                    // In-place rename: Enter saves, Esc cancels
                    on_escape(
                        text_input("Folder name", name)
                            .id(text_input::Id::new(RENAME_FOLDER_INPUT_ID))
                            .on_input(Message::RenameFolderInputChanged)
                            .on_submit(Message::SubmitRenameFolder)
                            .size(13)
                            .padding([3, 6])
                            .width(Length::Fill),
                        Message::CancelRenameFolder,
                    )
                    .into()
                } else {
                    button(
                    row![
                        row![
                            icon(
                                Icon::Folder,
                                13.0,
                                if is_folder_active { Tint::OnPrimary } else { Tint::Text },
                            ),
                            text(&folder.name).size(13),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .width(Length::Fill),
                        if !unread_badge.is_empty() {
                            text(unread_badge).size(11).style(text::primary)
                        } else {
                            text("")
                        },
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([4, 6])
                .style(if is_folder_active {
                    button::primary
                } else {
                    button::text
                })
                .on_press(Message::SelectFilter(ActiveFilter::Folder(folder.id)))
                .into()
                };

                let folder_header_row = row![
                    Element::from(toggle_btn),
                    folder_btn,
                ]
                .spacing(2)
                .align_y(Alignment::Center);

                let folder_header = container(folder_header_row)
                    .width(Length::Fill)
                    .style(drop_zone_style(is_dragging, is_drop_hovered));

                let mut folder_drop_area = mouse_area(folder_header);
                if is_dragging {
                    folder_drop_area = folder_drop_area
                        .on_enter(Message::DragOverTarget(DropTarget::Folder(Some(folder.id))))
                        .on_exit(Message::DragLeaveTarget(DropTarget::Folder(Some(folder.id))))
                        .on_release(Message::DropOnTarget(DropTarget::Folder(Some(folder.id))))
                        .interaction(mouse::Interaction::Grabbing);
                }

                let menu = self
                    .folder_menu
                    .filter(|m| m.folder_id == folder.id && !is_dragging)
                    .map(|m| self.folder_menu_view(folder, m.confirm_delete));

                let mut header_with_menu = context_menu(folder_drop_area)
                    .menu(menu)
                    .on_dismiss(Message::CloseFolderMenu)
                    .on_escape(Message::EscapePressed);
                if !is_dragging {
                    header_with_menu =
                        header_with_menu.on_right_press(Message::OpenFolderMenu(folder.id));
                }

                tree_items = tree_items.push(header_with_menu);
            }

            // Nested Feeds under this folder
            if is_expanded {
                let feeds_in_folder: Vec<&Feed> = self
                    .feeds
                    .iter()
                    .filter(|f| f.folder_id == Some(folder.id))
                    .collect();

                if feeds_in_folder.is_empty() {
                    tree_items = tree_items.push(
                        row![
                            Space::with_width(18),
                            text("(empty folder)").size(11).style(text::secondary),
                        ]
                    );
                } else {
                    for feed in feeds_in_folder {
                        tree_items = tree_items.push(self.feed_row(feed, true));
                    }
                }
            }
        }

        // 2. Uncategorized Drop Target & Feeds (Drag feeds OUT of folders into here!)
        let uncategorized_feeds: Vec<&Feed> = self
            .feeds
            .iter()
            .filter(|f| f.folder_id.is_none())
            .collect();

        tree_items = tree_items.push(Space::with_height(8));

        {
            // Uncategorized header: same size whether or not a drag is in progress
            // (single line, no wrapping) so the feed rows below it don't move.
            let is_over_uncategorized =
                is_dragging && self.drag_hover_target == Some(DropTarget::Folder(None));

            let uncat_header = container(
                text(if is_dragging {
                    "UNCATEGORIZED · drop here to remove from folder"
                } else {
                    "UNCATEGORIZED"
                })
                .size(10)
                .wrapping(text::Wrapping::None)
                .style(if is_over_uncategorized { text::primary } else { text::secondary }),
            )
            .padding([5, 6])
            .width(Length::Fill)
            .style(drop_zone_style(is_dragging, is_over_uncategorized));

            let mut uncat_drop_area = mouse_area(uncat_header);
            if is_dragging {
                uncat_drop_area = uncat_drop_area
                    .on_enter(Message::DragOverTarget(DropTarget::Folder(None)))
                    .on_exit(Message::DragLeaveTarget(DropTarget::Folder(None)))
                    .on_release(Message::DropOnTarget(DropTarget::Folder(None)))
                    .interaction(mouse::Interaction::Grabbing);
            }

            tree_items = tree_items.push(uncat_drop_area);
        }

        for feed in uncategorized_feeds {
            tree_items = tree_items.push(self.feed_row(feed, false));
        }

        // Reserve a gutter so the scrollbar sits beside rows instead of overlapping their right edge
        let tree_scroll = scrollable(tree_items).spacing(4).height(Length::Fill);

        // Bottom Storage & SQLite Archival Status
        let db_status = column![
            horizontal_rule(1),
            row![
                text("SQLite:").size(11).style(text::secondary),
                text("WAL Mode (Safe)").size(11).style(text::success),
            ]
            .spacing(4),
            text(format!(
                "{} folders | {} feeds | {} stories",
                self.folders.len(), total_feeds, total_articles
            ))
            .size(11)
            .style(text::secondary),
        ]
        .spacing(4);

        container(
            column![
                title_bar,
                horizontal_rule(1),
                refresh_btn,
                horizontal_rule(1),
                categories,
                horizontal_rule(1),
                subscriptions_header,
                tree_scroll,
                db_status,
            ]
            .spacing(10)
            .padding(10),
        )
        .width(Length::Fixed(290.0))
        .height(Length::Fill)
        .into()
    }

    fn filter_button(
        &self,
        target_filter: ActiveFilter,
        leading_icon: Option<Icon>,
        label: &str,
        badge: &str,
    ) -> Element<'_, Message> {
        let is_selected = self.active_filter == target_filter;
        let label_owned = label.to_string();
        let badge_owned = badge.to_string();

        let mut label_row = row![].spacing(6).align_y(Alignment::Center).width(Length::Fill);
        if let Some(leading_icon) = leading_icon {
            label_row = label_row.push(icon(
                leading_icon,
                13.0,
                if is_selected { Tint::OnPrimary } else { Tint::Text },
            ));
        }
        label_row = label_row.push(text(label_owned).size(13));

        button(
            row![
                label_row,
                if !badge.is_empty() {
                    text(badge_owned).size(12).style(text::secondary)
                } else {
                    text("")
                },
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding([6, 10])
        .style(if is_selected {
            button::primary
        } else {
            button::text
        })
        .on_press(Message::SelectFilter(target_filter))
        .into()
    }

    fn view_article_list(&self) -> Element<'_, Message> {
        // Search & Filter Header
        let search_box = text_input("Search stories (FTS5 full text)...", &self.search_query)
            .on_input(Message::SearchChanged)
            .padding(6)
            .size(13);

        let list_header_label = match self.active_filter {
            ActiveFilter::All => format!("All Stories ({})", self.articles.len()),
            ActiveFilter::Unread => format!("Unread Stories ({})", self.articles.len()),
            ActiveFilter::Starred => format!("Starred Stories ({})", self.articles.len()),
            ActiveFilter::Folder(foid) => {
                let name = self
                    .folders
                    .iter()
                    .find(|f| f.id == foid)
                    .map(|f| f.name.as_str())
                    .unwrap_or("Folder");
                format!("{} ({})", name, self.articles.len())
            }
            ActiveFilter::Feed(fid) => {
                let name = self
                    .feeds
                    .iter()
                    .find(|f| f.id == fid)
                    .map(|f| f.title.as_str())
                    .unwrap_or("Feed");
                format!("{} ({})", name, self.articles.len())
            }
        };

        let mut header_label = row![].spacing(6).align_y(Alignment::Center).width(Length::Fill);
        if matches!(self.active_filter, ActiveFilter::Folder(_)) {
            header_label = header_label.push(icon(Icon::Folder, 14.0, Tint::Text));
        }
        header_label = header_label.push(text(list_header_label).size(14));

        let header_row = row![
            header_label,
            button(text("Mark All Read").size(12))
                .padding([4, 8])
                .style(button::secondary)
                .on_press(Message::MarkAllRead),
        ]
        .align_y(Alignment::Center);

        // List of Articles
        let mut article_items = column![].spacing(6);

        if self.articles.is_empty() {
            let empty_msg = if !self.search_query.is_empty() {
                "No stories match your search query."
            } else if self.feeds.is_empty() {
                "No feeds added yet. Click + Feed on the left to start archiving!"
            } else {
                "No stories found in this view."
            };

            article_items = article_items.push(
                container(text(empty_msg).size(13).style(text::secondary))
                    .padding(20)
                    .center_x(Length::Fill),
            );
        } else {
            for article in self.articles.iter().take(self.article_render_limit) {
                let is_selected = self.selected_article_id == Some(article.id);

                let feed_name = article.feed_title.as_deref().unwrap_or("RSS");
                let date_str = article
                    .published_at
                    .map(format_date)
                    .unwrap_or_else(|| "Unknown date".to_string());

                let star_btn = button(if article.is_starred {
                    icon(Icon::StarFilled, 14.0, Tint::Gold)
                } else {
                    icon(Icon::Star, 14.0, Tint::Secondary)
                })
                    .padding([2, 6])
                    .style(button::text)
                    .on_press(Message::ToggleStar(article.id));

                let card_title = text(&article.title)
                    .size(13)
                    .style(if !article.is_read {
                        text::primary
                    } else {
                        text::secondary
                    });

                let meta_row = row![
                    text(feed_name).size(11).style(text::secondary),
                    text("•").size(11).style(text::secondary),
                    text(date_str).size(11).style(text::secondary),
                ]
                .spacing(4);

                let card_content = column![card_title, meta_row].spacing(3).width(Length::Fill);

                let item_btn = button(row![star_btn, card_content].spacing(6).align_y(Alignment::Center))
                    .width(Length::Fill)
                    .padding(8)
                    .style(if is_selected {
                        button::primary
                    } else {
                        button::secondary
                    })
                    .on_press(Message::SelectArticle(article.id));

                let menu = (self.article_menu == Some(article.id)).then(|| article_menu_view(article));
                article_items = article_items.push(
                    context_menu(item_btn)
                        .menu(menu)
                        .on_right_press(Message::OpenArticleMenu(article.id))
                        .on_dismiss(Message::CloseArticleMenu),
                );
            }

            let remaining = self.articles.len().saturating_sub(self.article_render_limit);
            if remaining > 0 {
                article_items = article_items.push(
                    button(
                        text(format!(
                            "Show {} more ({} remaining)",
                            remaining.min(ARTICLE_PAGE_SIZE),
                            remaining
                        ))
                        .size(12)
                        .width(Length::Fill)
                        .align_x(iced::alignment::Horizontal::Center),
                    )
                    .width(Length::Fill)
                    .padding(8)
                    .style(button::text)
                    .on_press(Message::ShowMoreArticles),
                );
            }
        }

        let scroll_list = scrollable(article_items).width(Length::Fill).height(Length::Fill);

        container(
            column![search_box, header_row, horizontal_rule(1), scroll_list]
                .spacing(8)
                .padding(10),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn view_reader(&self) -> Element<'_, Message> {
        let selected_article = self
            .selected_article_id
            .and_then(|id| self.articles.iter().find(|a| a.id == id).cloned());

        match selected_article {
            None => container(
                column![
                    text("No Story Selected").size(18),
                    text("Select an article from the list to view its contents.")
                        .size(13)
                        .style(text::secondary),
                    Space::with_height(10),
                    row![
                        icon(Icon::Lock, 12.0, Tint::Secondary),
                        text("Archival Guarantee: Every story fetched is stored in SQLite and will remain accessible even if the source feed removes it.")
                            .size(12)
                            .style(text::secondary),
                    ]
                    .spacing(6),
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),

            Some(art) => {
                // Header details
                // Before fields are moved out of `art` below
                let body = match &self.reader_body {
                    Some((id, blocks)) if *id == art.id => self.story_body(blocks, true),
                    // Not prepared yet (bodies are loaded when a story is opened)
                    _ => Space::with_height(0).into(),
                };
                let title = text(art.title).size(22);

                let feed = self.feeds.iter().find(|f| f.id == art.feed_id);
                let feed_title = art.feed_title.unwrap_or_else(|| "Feed".to_string());
                let folder_name = feed
                    .and_then(|f| f.folder_id)
                    .and_then(|foid| self.folders.iter().find(|fld| fld.id == foid))
                    .map(|fld| fld.name.clone());

                let date_str = art
                    .published_at
                    .map(format_date)
                    .unwrap_or_else(|| "Unknown date".to_string());

                let mut meta_items = row![
                    text(format!("Feed: {}", feed_title)).size(12).style(text::secondary),
                ]
                .spacing(6);

                if let Some(ref fld) = folder_name {
                    meta_items = meta_items.push(text("•").size(12).style(text::secondary));
                    meta_items = meta_items.push(
                        row![
                            icon(Icon::Folder, 12.0, Tint::Secondary),
                            text(fld.clone()).size(12).style(text::secondary),
                        ]
                        .spacing(4)
                        .align_y(Alignment::Center),
                    );
                }

                meta_items = meta_items.push(text("•").size(12).style(text::secondary));
                meta_items = meta_items.push(text(format!("Date: {}", date_str)).size(12).style(text::secondary));

                if let Some(author) = art.author {
                    meta_items = meta_items.push(text("•").size(12).style(text::secondary));
                    meta_items = meta_items.push(text(format!("By: {}", author)).size(12).style(text::secondary));
                }

                // Actions toolbar
                let mut action_row = row![
                    button(text(if art.is_read { "Mark Unread" } else { "Mark Read" }).size(12))
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::ToggleRead(art.id)),
                    button(
                        row![
                            if art.is_starred {
                                icon(Icon::StarFilled, 12.0, Tint::Gold)
                            } else {
                                icon(Icon::Star, 12.0, Tint::OnSecondary)
                            },
                            text(if art.is_starred { "Starred" } else { "Star" }).size(12),
                        ]
                        .spacing(5)
                        .align_y(Alignment::Center),
                    )
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::ToggleStar(art.id)),
                ]
                .spacing(8);

                if let Some(url) = art.url {
                    let open_btn = button(
                        row![
                            icon(Icon::ExternalLink, 12.0, Tint::OnPrimary),
                            text("Open in Browser").size(12),
                        ]
                        .spacing(5)
                        .align_y(Alignment::Center),
                    )
                        .padding([4, 10])
                        .style(button::primary)
                        .on_press(Message::OpenUrl(url));
                    action_row = action_row.push(open_btn);
                }

                // Fill the reader's width: a shrink-sized scrollable is only as wide
                // as its text, which leaves the scrollbar mid-window for short stories
                // The scrollable spans the reader (scrollbar at the window edge); the
                // text inside it is a centered column
                let body_scroll = scrollable(reading_column(
                    column![body].spacing(10).padding([10, 0]),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(10).scroller_width(10).spacing(4),
                ));

                // Footer with permanent archival info
                let footer = column![
                    horizontal_rule(1),
                    row![
                        text(format!("Permanently archived in SQLite (ID: #{})", art.id))
                            .size(11)
                            .style(text::secondary),
                        horizontal_space(),
                        text(format!("Fetched: {}", art.fetched_at.format("%Y-%m-%d %H:%M UTC")))
                            .size(11)
                            .style(text::secondary),
                    ],
                ]
                .spacing(4);

                container(
                    column![
                        gutter(reading_column(
                            // Wrap so a narrow reader moves items to a new line instead of squeezing them
                            column![title, meta_items.wrap(), action_row.wrap(), horizontal_rule(1)]
                                .spacing(12),
                        )),
                        body_scroll,
                        gutter(reading_column(footer)),
                    ]
                    .spacing(12)
                    .padding(20),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
            }
        }
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn format_date(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(dt);

    if diff.num_minutes() < 60 {
        format!("{}m ago", diff.num_minutes().max(1))
    } else if diff.num_hours() < 24 {
        format!("{}h ago", diff.num_hours())
    } else if diff.num_days() < 7 {
        format!("{}d ago", diff.num_days())
    } else {
        dt.format("%b %d, %Y").to_string()
    }
}

/// Drag handle drawn as a 2x3 grid of dots. Built from widgets rather than a
/// glyph like "⠿", which the default font doesn't contain (renders as tofu).
fn grip_icon<'a>(active: bool) -> Element<'a, Message> {
    let dot = move || {
        container(Space::new(Length::Fixed(3.0), Length::Fixed(3.0))).style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color = if active {
                palette.primary.base.color
            } else {
                palette.secondary.strong.color
            };
            container::Style {
                background: Some(color.into()),
                border: iced::border::rounded(1.5),
                ..Default::default()
            }
        })
    };
    let dot_row = move || row![dot(), dot()].spacing(2);
    column![dot_row(), dot_row(), dot_row()].spacing(2).into()
}

/// Style for folder / Uncategorized headers. It only changes colors (borders
/// don't affect layout), so starting or ending a drag never resizes anything.
fn drop_zone_style(
    is_dragging: bool,
    is_hovered: bool,
) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        if !is_dragging {
            return container::Style::default();
        }
        let palette = theme.extended_palette();
        let (background, border_color) = if is_hovered {
            (palette.primary.weak.color, palette.primary.strong.color)
        } else {
            (palette.background.weak.color, palette.background.strong.color)
        };
        container::Style {
            background: Some(background.into()),
            border: iced::Border {
                color: border_color,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        }
    }
}

/// The floating card that holds a right-click menu.
fn menu_card<'a>(body: impl Into<Element<'a, Message>>, width: f32) -> Element<'a, Message> {
    container(body)
        .width(Length::Fixed(width))
        .padding(4)
        .style(|theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(palette.background.base.color.into()),
                border: iced::Border {
                    color: palette.background.strong.color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.4),
                    offset: Vector::new(0.0, 3.0),
                    blur_radius: 10.0,
                },
                ..Default::default()
            }
        })
        .into()
}

/// A dialog card over a dimmed backdrop that blocks the app behind it.
/// Esc publishes `on_cancel`.
fn modal<'a>(form: impl Into<Element<'a, Message>>, on_cancel: Message) -> Element<'a, Message> {
    let card = container(form)
        .width(Length::Fixed(440.0))
        .padding(18)
        .style(|theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(palette.background.base.color.into()),
                border: iced::Border {
                    color: palette.background.strong.color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5),
                    offset: Vector::new(0.0, 6.0),
                    blur_radius: 24.0,
                },
                ..Default::default()
            }
        });

    // The backdrop swallows clicks so nothing underneath reacts while the dialog
    // is open. It deliberately doesn't cancel: `stack` passes clicks on blank
    // parts of the card down to this layer, and a stray click shouldn't throw
    // away edits. Cancel / Esc close the dialog.
    let backdrop = mouse_area(
        container(Space::new(Length::Fill, Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
                ..Default::default()
            }),
    )
    .on_press(Message::Noop)
    .on_right_press(Message::Noop)
    .on_middle_press(Message::Noop)
    .interaction(mouse::Interaction::Idle);

    // on_escape: a focused text field would otherwise swallow Esc
    on_escape(stack![
        backdrop,
        // Anchored near the top rather than vertically centered, so when an
        // error line appears the card grows downward and the fields and
        // buttons stay where they are.
        container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .padding(iced::Padding::ZERO.top(140)),
    ], on_cancel)
    .into()
}

fn uncategorized_choice() -> FolderChoice {
    FolderChoice { id: None, name: "Uncategorized".to_string() }
}

/// Turns typed input into a feed URL: adds https:// when no scheme was typed,
/// and accepts only http(s) URLs with a host.
fn normalize_feed_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.contains(char::is_whitespace) {
        return None;
    }
    let candidate = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{}", input)
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    let host = parsed.host_str()?;
    // "https://foo" alone is almost certainly still being typed
    if !matches!(parsed.scheme(), "http" | "https") || !(host.contains('.') || host == "localhost") {
        return None;
    }
    Some(candidate)
}

/// Centers `content` in a column at most `READER_TEXT_MAX_WIDTH` wide, so the
/// story doesn't hug the left edge of a wide reader pane.
fn reading_column<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(container(content).width(Length::Fill).max_width(READER_TEXT_MAX_WIDTH))
        .center_x(Length::Fill)
        .into()
}


/// The story as text and image blocks for the reader.
///
/// Built from the HTML rather than the stored `content_text`: that one is
/// hard-wrapped at 80 columns (it's made for search) and has no images.
fn reader_blocks(article: &Article) -> Vec<Block> {
    let blocks = article
        .content_html
        .as_deref()
        .or(article.summary.as_deref())
        .map(|html| story_content::blocks(html, article.url.as_deref()))
        .unwrap_or_default();
    if !blocks.is_empty() {
        return blocks;
    }
    vec![Block::Text(
        article
            .content_text
            .clone()
            .unwrap_or_else(|| "No content available for this story.".to_string()),
    )]
}

/// Leaves room on the right matching the story's scrollbar (see `READER_SCROLLBAR_GUTTER`).
fn gutter<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(iced::Padding::ZERO.right(READER_SCROLLBAR_GUTTER))
        .into()
}

/// Contents of a story's right-click menu. Items that wouldn't change anything
/// (or have nothing to open) are shown disabled.
fn article_menu_view(article: &Article) -> Element<'_, Message> {
    let menu_item = |label: &'static str, message: Option<Message>| {
        button(text(label).size(13))
            .width(Length::Fill)
            .padding([5, 10])
            .style(button::text)
            .on_press_maybe(message)
    };
    let body = column![
        menu_item(
            "Mark read",
            (!article.is_read).then_some(Message::SetArticleRead(article.id, true)),
        ),
        menu_item(
            "Mark unread",
            article.is_read.then_some(Message::SetArticleRead(article.id, false)),
        ),
        horizontal_rule(1),
        menu_item(
            "Open in browser",
            article.url.clone().map(Message::OpenArticleInBrowser),
        ),
    ]
    .spacing(2);
    menu_card(body, 170.0)
}
