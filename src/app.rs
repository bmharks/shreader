use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use chrono::{DateTime, Utc};
use iced::mouse;
use iced::widget::{
    button, checkbox, column, container, horizontal_rule, horizontal_space, mouse_area, pick_list, row,
    scrollable, stack, text, text_editor, text_input, vertical_rule, Space,
};
use iced::{Alignment, Color, Element, Length, Subscription, Task, Theme, Vector};

use crate::db::Database;
use crate::opml;
use crate::feed::{FeedFetcher, FeedPreview, StoryImage};
use crate::story_content::{self, Block, LinkSpan};
use crate::context_menu::context_menu;
use crate::follow_cursor::follow_cursor;
use crate::key_press::{on_key_press, scroll_by_step, scroll_into_view};
use crate::lazy_load::on_visible;
use crate::image_space::image_space;
use crate::story_text::{find_matches, story_text, TextRange};
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

/// Memory the open story's images may use once decoded. Each can take up to
/// ~30 MB (1600x4800), and a story can hold hundreds; past this, the images
/// loaded longest ago (normally those farthest from where you're reading)
/// are released and reload if scrolled back to.
const STORY_IMAGE_MEMORY: u64 = 400 * 1024 * 1024;

/// Story images downloaded at once. Each download can hold up to 20 MB
/// until it's decoded, and a story can ask for 100+ images in one go.
const MAX_IMAGE_DOWNLOADS: usize = 4;

/// Setting: download story images as they scroll into view (default), or
/// only when clicked
const LOAD_IMAGES_SETTING: &str = "load_images";

/// Setting: closing the window keeps Shreader running in the system tray
/// (default), refreshing in the background
const KEEP_IN_TRAY_SETTING: &str = "keep_in_tray";

/// Setting: the story list shows groups of stories about the same thing
const GROUPED_SETTING: &str = "grouped_view";

/// Setting: the Ollama model that labels training examples
const OLLAMA_MODEL_SETTING: &str = "ollama_model";

/// Setting: list unread stories before read ones
const UNREAD_FIRST_SETTING: &str = "unread_first";

/// Setting: show messages in the corner of the window (default), or none
const NOTIFICATIONS_SETTING: &str = "notifications";

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
/// Settings key: comma-separated ids of folders the user collapsed. Collapsed
/// rather than expanded ones are stored, so new folders start out open.
const COLLAPSED_FOLDERS_SETTING: &str = "collapsed_folders";
/// Settings keys for automatic refreshing: on/off, and the interval in minutes.
const AUTO_REFRESH_SETTING: &str = "auto_refresh";
const AUTO_REFRESH_MINUTES_SETTING: &str = "auto_refresh_minutes";
/// Akregator's defaults: "Use interval fetching" on, every 30 minutes.
const DEFAULT_AUTO_REFRESH_MINUTES: u32 = 30;
/// Intervals offered in Settings, in minutes.
const AUTO_REFRESH_CHOICES: [u32; 10] = [5, 10, 15, 30, 60, 120, 240, 360, 720, 1440];

/// An automatic refresh interval, shown as "30 minutes", "2 hours"...
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshInterval(u32);

impl std::fmt::Display for RefreshInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let m = self.0;
        let (n, unit) = if m % 1440 == 0 {
            (m / 1440, "day")
        } else if m % 60 == 0 {
            (m / 60, "hour")
        } else {
            (m, "minute")
        };
        write!(f, "{} {}{}", n, unit, if n == 1 { "" } else { "s" })
    }
}

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
    /// How many stories are loaded into `articles` (and shown)
    article_render_limit: usize,
    /// How many stories the current view has in all
    article_total: usize,
    /// The open story's list entry (no body; see `reader_body`), while it's
    /// part of the current view
    open_article: Option<Article>,
    expanded_folders: HashSet<i64>,
    active_filter: ActiveFilter,
    selected_article_id: Option<i64>,
    /// The selected story split into text and images, converted once when
    /// it's opened (too slow to redo on every view rebuild)
    reader_body: Option<(i64, Vec<Block>)>,
    /// Images of the open story, by URL
    story_images: HashMap<String, ImageState>,
    /// Counts image loads, for `ImageState::Raster::order`
    image_loads: u64,
    /// Images waiting to download, in the order they were asked for
    image_queue: VecDeque<String>,
    /// Downloads in progress. Dropping a handle cancels its download.
    image_downloads: HashMap<String, iced::task::Handle>,
    /// Whether story images load automatically (`LOAD_IMAGES_SETTING`)
    load_images: bool,
    /// Whether messages pop up in the corner (`NOTIFICATIONS_SETTING`)
    notifications: bool,
    /// Whether the story list shows unread stories first (`UNREAD_FIRST_SETTING`)
    unread_first: bool,
    /// Whether the story list is grouped (`GROUPED_SETTING`)
    grouped: bool,
    /// The loaded part of the list as groups, while `grouped`
    story_groups: Vec<StoryGroupEntry>,
    /// The group open in the reader (instead of a single story)
    reader_group: Option<ReaderGroup>,
    /// Story embeddings: whether they're ready
    embeddings: EmbeddingStatus,
    ollama: crate::ollama::Ollama,
    /// The models installed in Ollama, or why it couldn't be reached (checked
    /// when Settings opens)
    ollama_models: Option<Result<Vec<String>, String>>,
    ollama_model: String,
    labelling: Option<Labelling>,
    /// Pairs Ollama has labelled, and how many it judged the same story
    ollama_labels: (i64, i64),
    /// The learned grouping model, once trained (see `training`)
    model_info: Option<crate::training::ModelInfo>,
    /// The model is being trained (and recent stories regrouped)
    training_model: bool,
    /// The main window, while it's open (it isn't while running in the tray)
    main_window: Option<iced::window::Id>,
    /// Whether the desktop shows Shreader's tray icon
    tray: bool,
    /// Whether closing the window keeps the app in the tray (`KEEP_IN_TRAY_SETTING`)
    keep_in_tray: bool,
    /// The open story's text blocks as read-only editors, so text can be
    /// selected and copied (iced's plain `text` can't be selected)
    reader_texts: Vec<text_editor::Content>,
    /// The links in each of `reader_texts`
    reader_links: Vec<std::sync::Arc<Vec<LinkSpan>>>,
    /// A link pressed in the story text: opened when the button is released,
    /// unless the press turned into a drag (selecting text)
    pressed_link: Option<String>,
    /// The link under the right click that opened the story text's menu
    menu_link: Option<String>,
    /// The find bar, while it's open
    find: Option<FindState>,
    /// Text block whose right-click (Copy / Select all) menu is open
    reader_text_menu: Option<usize>,
    search_query: String,
    new_folder_input: String,
    show_new_folder_input: bool,
    /// Refreshes (and new-feed downloads) in progress. A count, not a flag:
    /// adding a feed during a full refresh runs both at once, and the first
    /// to finish mustn't make the app think nothing is running.
    syncs_running: u32,
    status_message: Option<(String, bool)>,
    /// Bumped whenever a new status message appears; see `STATUS_TIMEOUT`
    status_generation: u64,
    stats: (i64, i64, i64), // (total_feeds, total_articles, unread_articles)
    /// Starred stories in the whole archive (for the sidebar)
    starred_total: i64,

    // Drag-and-drop state
    dragging_feed: Option<i64>,                 // feed_id being dragged
    dragging_folder: Option<i64>,               // folder_id being dragged
    /// Refresh all feeds every `auto_refresh_minutes` (Settings)
    auto_refresh: bool,
    auto_refresh_minutes: u32,

    // Feed context menu (right click) and in-place rename
    feed_menu: Option<FeedMenu>,
    renaming_feed: Option<(i64, String)>,       // (feed_id, edited title)
    editing_feed: Option<EditFeedDialog>,

    add_feed: Option<AddFeedDialog>,
    /// The Settings window, while open
    settings: Option<SettingsState>,
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
    /// Photos, and SVGs drawn into pixels in the background. `order` says
    /// when it loaded, so the oldest are released first.
    Raster { handle: iced::widget::image::Handle, width: u32, height: u32, order: u64 },
    /// Released to keep memory in check (`STORY_IMAGE_MEMORY`). Its space is
    /// kept, and it loads again when scrolled near.
    Released { width: u32, height: u32 },
    /// Released, and downloading again
    Reloading { width: u32, height: u32 },
    Failed,
}

/// One entry of the grouped story list: a group's stories in the current
/// view, newest first (one story when nothing else matched it)
#[derive(Debug, Clone)]
struct StoryGroupEntry {
    group: i64,
    members: Vec<Article>,
}

impl StoryGroupEntry {
    /// The first report's headline
    fn title(&self) -> &str {
        self.members.last().map_or("", |a| a.title.as_str())
    }

    fn unread(&self) -> usize {
        self.members.iter().filter(|a| !a.is_read).count()
    }
}

/// A group open in the reader: its stories oldest first, with their bodies
#[derive(Debug, Clone)]
struct ReaderGroup {
    group: i64,
    title: String,
    stories: Vec<(Article, Vec<Block>)>,
}

/// Where the story embeddings stand (see `embeddings`)
#[derive(Debug, Clone, PartialEq)]
enum EmbeddingStatus {
    /// Not needed yet: grouping hasn't been used
    Idle,
    /// Downloading the model (the first time) and/or analysing stories
    Working,
    /// Up to date: this many stories have embeddings
    Ready(i64),
    Failed(String),
}

/// A labelling run in progress (see `training::label_with_ollama`)
struct Labelling {
    status: crate::training::LabelEvent,
    /// Dropping it stops the run
    _handle: iced::task::Handle,
}

/// Find in story (Ctrl+F)
#[derive(Debug, Clone, Default)]
struct FindState {
    query: String,
    /// The matches in each of the story's text blocks
    matches: Vec<Vec<TextRange>>,
    /// Index of the current match, counting through all blocks
    current: usize,
}

impl FindState {
    fn total(&self) -> usize {
        self.matches.iter().map(Vec::len).sum()
    }

    /// The current match as (text block, index within it)
    fn current_location(&self) -> Option<(usize, usize)> {
        let mut remaining = self.current;
        for (block, matches) in self.matches.iter().enumerate() {
            if remaining < matches.len() {
                return Some((block, remaining));
            }
            remaining -= matches.len();
        }
        None
    }
}

/// Most matches found in one story (finding "e" in a long story could
/// otherwise mean tens of thousands of highlights)
const MAX_FIND_MATCHES: usize = 5000;

/// Id of the find bar's text field
const FIND_INPUT_ID: &str = "find-in-story";

/// State of the Settings window.
#[derive(Debug, Clone)]
struct SettingsState {
    /// "Keep starred stories" when clearing
    keep_starred: bool,
    /// While asking to confirm clearing: (stories to delete, starred kept)
    confirm_clear: Option<(i64, i64)>,
    /// A file chooser (import or export) is open
    choosing_file: bool,
    /// Outcome of the last import: (message, is_error)
    import_result: Option<(String, bool)>,
    /// Outcome of the last export: (message, is_error)
    export_result: Option<(String, bool)>,
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
    /// Left/Right arrow: open the previous (`false`) or next (`true`) story
    /// in the list
    SelectAdjacentStory(bool),
    /// Up/Down arrow: scroll the open story by this many pixels
    ScrollReader(f32),
    FeedAdded(Result<(i64, String), String>),
    RefreshClicked,
    SyncFinished(Result<Vec<FeedSyncSummary>, String>),
    /// The automatic refresh timer fired
    AutoRefreshTick,
    SetAutoRefresh(bool),
    SetLoadImages(bool),
    SetNotifications(bool),
    ToggleUnreadFirst,
    ToggleGrouped,
    /// Open a group of stories in the reader
    SelectGroup(i64),
    /// Background grouping finished: how many stories it grouped
    StoriesGrouped(Result<usize, String>),
    /// Background embedding finished: how many stories it embedded
    StoriesEmbedded(Result<usize, String>),
    OllamaModels(Result<Vec<String>, String>),
    SetOllamaModel(String),
    StartLabelling,
    StopLabelling,
    Labelling(crate::training::LabelEvent),
    ModelTrained(Result<Option<crate::training::ModelInfo>, String>),
    /// The window's close button
    WindowCloseRequested,
    /// From the tray, or a second copy of the app
    Desktop(crate::desktop::Command),
    SetKeepInTray(bool),
    Quit,
    OpenFind,
    FindQueryChanged(String),
    FindNext,
    FindPrevious,
    CloseFind,
    SetAutoRefreshInterval(RefreshInterval),
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
    /// Right click in story text block `usize`, at this point in it
    OpenReaderTextMenu(usize, iced::Point),
    /// The left button was released over the story text
    ReaderMouseReleased,
    CopyLink(String),
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

    // Settings window
    OpenSettings,
    CloseSettings,
    SetKeepStarred(bool),
    AskClearStories,
    CancelClearStories,
    ClearStories,
    ImportOpml,
    /// The OPML file chooser closed: `None` if cancelled, else (file name, contents)
    OpmlFilePicked(Option<Result<(String, String), String>>),
    ExportOpml,
    /// The export file chooser closed: `None` if cancelled, else the saved
    /// file's name and how many feeds it holds, or an error
    OpmlExported(Option<Result<(String, usize), String>>),
    /// Give memory freed by a closed story's images back to the system
    ReleaseMemory,

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
    DragFolderStarted(i64),
    DragOverTarget(DropTarget),
    DragLeaveTarget(DropTarget),
    DropOnTarget(DropTarget),
    DragFeedDropped,
}

impl RssApp {
    /// `tray`: whether the tray icon is showing (see `desktop::start_tray`).
    pub fn new(db: Database, tray: bool) -> (Self, Task<Message>) {
        let fetcher = Arc::new(FeedFetcher::new());
        let model_info = crate::training::ModelInfo::load(&db);

        let mut app = Self {
            db,
            fetcher,
            folders: Vec::new(),
            feeds: Vec::new(),
            articles: Vec::new(),
            article_render_limit: ARTICLE_PAGE_SIZE,
            article_total: 0,
            open_article: None,
            expanded_folders: HashSet::new(),
            active_filter: ActiveFilter::All,
            selected_article_id: None,
            reader_body: None,
            story_images: HashMap::new(),
            image_loads: 0,
            image_queue: VecDeque::new(),
            image_downloads: HashMap::new(),
            load_images: true,
            notifications: true,
            unread_first: false,
            grouped: false,
            story_groups: Vec::new(),
            reader_group: None,
            embeddings: EmbeddingStatus::Idle,
            ollama: crate::ollama::Ollama::new(),
            ollama_models: None,
            ollama_model: crate::ollama::DEFAULT_MODEL.to_string(),
            labelling: None,
            ollama_labels: (0, 0),
            model_info,
            training_model: false,
            main_window: None,
            tray,
            keep_in_tray: true,
            reader_texts: Vec::new(),
            reader_links: Vec::new(),
            pressed_link: None,
            menu_link: None,
            find: None,
            reader_text_menu: None,
            search_query: String::new(),
            new_folder_input: String::new(),
            show_new_folder_input: false,
            syncs_running: 0,
            status_message: None,
            status_generation: 0,
            stats: (0, 0, 0),
            starred_total: 0,
            dragging_feed: None,
            dragging_folder: None,
            auto_refresh: true,
            auto_refresh_minutes: DEFAULT_AUTO_REFRESH_MINUTES,
            feed_menu: None,
            renaming_feed: None,
            editing_feed: None,
            add_feed: None,
            settings: None,
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

        if let Ok(Some(on)) = app.db.get_setting(LOAD_IMAGES_SETTING) {
            app.load_images = on != "false";
        }
        if let Ok(Some(on)) = app.db.get_setting(NOTIFICATIONS_SETTING) {
            app.notifications = on != "false";
        }
        app.unread_first = matches!(app.db.get_setting(UNREAD_FIRST_SETTING), Ok(Some(on)) if on == "true");
        app.grouped = matches!(app.db.get_setting(GROUPED_SETTING), Ok(Some(on)) if on == "true");
        if let Ok(Some(model)) = app.db.get_setting(OLLAMA_MODEL_SETTING) {
            app.ollama_model = model;
        }
        app.keep_in_tray = !matches!(app.db.get_setting(KEEP_IN_TRAY_SETTING), Ok(Some(on)) if on == "false");
        if let Ok(Some(on)) = app.db.get_setting(AUTO_REFRESH_SETTING) {
            app.auto_refresh = on != "false";
        }
        if let Some(minutes) = app
            .db
            .get_setting(AUTO_REFRESH_MINUTES_SETTING)
            .ok()
            .flatten()
            .and_then(|m| m.parse::<u32>().ok())
            .filter(|&m| m > 0)
        {
            app.auto_refresh_minutes = minutes;
        }

        // Folders open as they were left: all expanded except those the user
        // collapsed
        let collapsed: HashSet<i64> = app
            .db
            .get_setting(COLLAPSED_FOLDERS_SETTING)
            .ok()
            .flatten()
            .map(|ids| ids.split(',').filter_map(|id| id.trim().parse().ok()).collect())
            .unwrap_or_default();
        app.expanded_folders = app
            .folders
            .iter()
            .map(|f| f.id)
            .filter(|id| !collapsed.contains(id))
            .collect();

        // Start in the top folder
        if let Some(top) = app.folders.first() {
            app.active_filter = ActiveFilter::Folder(top.id);
            app.reload_articles();
        }

        // If feeds exist, automatically trigger an initial background sync
        let initial_task = if !app.feeds.is_empty() {
            let db_clone = app.db.clone();
            let fetcher_clone = app.fetcher.clone();
            app.syncs_running += 1;
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

        if app.grouped {
            app.reload_articles();
        }
        let open_window = app.open_main_window();
        // Groups stories not grouped yet (all of them, the first time). With
        // labels but no usable model (one from an older version), it's
        // trained first.
        let has_model = app
            .db
            .get_setting(crate::story_groups::MODEL_SETTING)
            .ok()
            .flatten()
            .and_then(|json| crate::pair_model::PairModel::from_json(&json))
            .is_some();
        let labelled = app.db.pair_labels().map(|l| l.len()).unwrap_or(0);
        let group = if !has_model && labelled >= crate::training::MIN_EXAMPLES {
            app.train_model()
        } else {
            app.group_stories()
        };
        (app, Task::batch([open_window, initial_task, group]))
    }

    /// Opens the main window (it's closed while the app runs in the tray).
    fn open_main_window(&mut self) -> Task<Message> {
        let (id, opened) = iced::window::open(crate::desktop::main_window_settings());
        self.main_window = Some(id);
        opened.map(|_| Message::Noop)
    }

    /// The window title
    pub fn title(&self, _window: iced::window::Id) -> String {
        "Shreader RSS Reader".to_string()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // No global event listeners: they're fed every mouse move and can overflow
        // their channel ("TrySendError { kind: Full }"). Drag release comes from
        // the floating badge (follow_cursor.rs); Esc for context menus from the
        // menu overlay (context_menu.rs), and for the rename fields and dialogs
        // from `on_escape` wrappers.
        //
        // The refresh timer is fine: it ticks once per interval. Changing the
        // interval makes a new subscription, which restarts the countdown.
        //
        // The tray and a second copy of the app send their requests through
        // `desktop::commands`.
        let refresh = if self.auto_refresh && self.auto_refresh_minutes > 0 {
            iced::time::every(std::time::Duration::from_secs(u64::from(self.auto_refresh_minutes) * 60))
                .map(|_| Message::AutoRefreshTick)
        } else {
            Subscription::none()
        };
        let desktop = Subscription::run(crate::desktop::commands).map(Message::Desktop);
        Subscription::batch([refresh, desktop])
    }

    fn reload_data(&mut self) {
        self.folders = self.db.get_folders().unwrap_or_default();
        self.feeds = self.db.get_feeds().unwrap_or_default();
        self.stats = self.db.get_total_stats().unwrap_or((0, 0, 0));
        self.starred_total = self.db.starred_count().unwrap_or(0);
        self.reload_articles();
    }

    /// The stories the list is showing, as a database filter (no limit).
    fn list_filter(&self) -> ArticleFilter {
        let mut filter = ArticleFilter {
            search_query: Some(self.search_query.clone()).filter(|q| !q.trim().is_empty()),
            unread_first: self.unread_first,
            ..Default::default()
        };
        match self.active_filter {
            ActiveFilter::All => {}
            ActiveFilter::Unread => filter.unread_only = true,
            ActiveFilter::Starred => filter.starred_only = true,
            ActiveFilter::Folder(folder_id) => filter.folder_id = Some(folder_id),
            ActiveFilter::Feed(feed_id) => filter.feed_id = Some(feed_id),
        }
        filter
    }

    /// Reloads the story list: only the part that's shown
    /// (`article_render_limit` stories), plus the total for the counts.
    /// Reading every matching story would take seconds once the archive
    /// holds a few hundred thousand.
    fn reload_articles(&mut self) {
        let filter = self.list_filter();
        self.article_total = self.db.count_articles(&filter).unwrap_or(0);
        self.load_list_page(&filter);

        // The open group's stories: their read and starred state may have changed
        if let Some(group) = &mut self.reader_group {
            for (story, _) in &mut group.stories {
                if let Ok(Some(fresh)) = self.db.get_article(story.id) {
                    story.is_read = fresh.is_read;
                    story.is_starred = fresh.is_starred;
                }
            }
        }

        // The open story stays open while it's part of this view, even if
        // newer stories pushed it past the loaded part: then the list loads
        // down to it, so the arrow keys continue from where it is.
        self.open_article = None;
        let Some(id) = self.selected_article_id else {
            return;
        };
        if let Some(article) = self.articles.iter().find(|a| a.id == id) {
            self.open_article = Some(article.clone());
            return;
        }
        let only_it = ArticleFilter { article_id: Some(id), ..filter.clone() };
        let Some(article) = self.db.get_articles(&only_it).ok().and_then(|a| a.into_iter().next()) else {
            return; // not in this view
        };
        if let Ok(position) = self.db.count_before(&filter, id) {
            self.article_render_limit = (position / ARTICLE_PAGE_SIZE + 1) * ARTICLE_PAGE_SIZE;
            self.load_list_page(&filter);
        }
        self.open_article = Some(article);
    }

    fn load_list_page(&mut self, filter: &ArticleFilter) {
        let page = ArticleFilter { limit: Some(self.article_render_limit), ..filter.clone() };
        self.articles = self.db.get_articles(&page).unwrap_or_default();
        self.story_groups = if self.grouped { self.build_groups(filter) } else { Vec::new() };
    }

    /// The loaded stories as groups, in list order (a group goes where its
    /// first story in the list is), each with all its stories in this view.
    fn build_groups(&self, filter: &ArticleFilter) -> Vec<StoryGroupEntry> {
        let mut order: Vec<i64> = Vec::new();
        let mut seen = HashSet::new();
        for article in &self.articles {
            if seen.insert(article.group()) {
                order.push(article.group());
            }
        }
        let mut members: HashMap<i64, Vec<Article>> = HashMap::new();
        // Groups of one need no lookup
        let shared: Vec<i64> = order
            .iter()
            .copied()
            .filter(|&group| self.articles.iter().any(|a| a.group() == group && a.group_id.is_some()))
            .collect();
        for article in self.db.group_members(filter, &shared).unwrap_or_default() {
            members.entry(article.group()).or_default().push(article);
        }
        order
            .into_iter()
            .map(|group| {
                let members = members.remove(&group).filter(|m| !m.is_empty()).unwrap_or_else(|| {
                    self.articles.iter().filter(|a| a.group() == group).cloned().collect()
                });
                StoryGroupEntry { group, members }
            })
            .collect()
    }

    /// Left / Right in the grouped list: the previous or next group (or
    /// lone story), moving the list by one like the plain list does.
    fn select_adjacent_group(&mut self, forward: bool) -> Task<Message> {
        // Which entry is open: a group, or a lone story
        let position = |app: &Self| {
            app.story_groups.iter().position(|entry| match (&app.reader_group, entry.members.as_slice()) {
                (Some(open), _) => open.group == entry.group,
                (None, [single]) => app.selected_article_id == Some(single.id),
                _ => false,
            })
        };
        // Stepping past the loaded part loads more
        if forward
            && position(self).is_some_and(|i| i + 1 == self.story_groups.len())
            && self.articles.len() < self.article_total
        {
            self.article_render_limit += ARTICLE_PAGE_SIZE;
            self.reload_articles();
        }
        let current = position(self);
        let target = match (current, forward) {
            (None, _) if !self.story_groups.is_empty() => 0,
            (Some(i), true) if i + 1 < self.story_groups.len() => i + 1,
            (Some(i), false) if i > 0 => i - 1,
            _ => return Task::none(),
        };
        self.feed_menu = None;
        self.folder_menu = None;
        self.article_menu = None;
        let card_id = |entry: &StoryGroupEntry| match entry.members.as_slice() {
            [single] => story_card_id(single.id),
            _ => group_card_id(entry.group),
        };
        let to = card_id(&self.story_groups[target]);
        let from = current.map(|i| card_id(&self.story_groups[i]));
        let select = match self.story_groups[target].members.as_slice() {
            [single] => Message::SelectArticle(single.id),
            _ => Message::SelectGroup(self.story_groups[target].group),
        };
        let select = self.update(select);
        let scroll = match from {
            Some(from) => scroll_by_step(article_list_scroll_id(), from, to),
            None => scroll_into_view(article_list_scroll_id(), to),
        };
        Task::batch([select, then_redraw(scroll)])
    }

    /// Embeds recent stories in the background (see `embeddings`),
    /// downloading the model the first time. Only once grouping is in use:
    /// nobody else downloads 91 MB for a feature they don't use.
    fn embed_stories(&mut self) -> Task<Message> {
        if !self.grouped || self.embeddings == EmbeddingStatus::Working {
            return Task::none();
        }
        self.embeddings = EmbeddingStatus::Working;
        let db = self.db.clone();
        let fetcher = self.fetcher.clone();
        let dir = crate::embeddings::model_dir(db.path());
        Task::perform(
            async move {
                crate::embeddings::ensure_model(&dir, |url, max| {
                    let fetcher = fetcher.clone();
                    async move { fetcher.download(url, max).await }
                })
                .await?;
                tokio::task::spawn_blocking(move || {
                    crate::embeddings::embed_new_stories(&db, &dir).map_err(|e| format!("{:#}", e))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::StoriesEmbedded,
        )
    }

    /// Groups new stories in the background (see `story_groups`), embedding
    /// them first when grouping is in use (the learned model needs them).
    fn group_stories(&mut self) -> Task<Message> {
        let embed = self.embed_stories();
        let db = self.db.clone();
        let group = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || crate::story_groups::group_new_stories(&db).map_err(|e| e.to_string()))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::StoriesGrouped,
        );
        embed.chain(group)
    }

    /// Trains the grouping model on the labels so far and regroups recent
    /// stories with it, in the background.
    fn train_model(&mut self) -> Task<Message> {
        if self.training_model {
            return Task::none();
        }
        self.training_model = true;
        let db = self.db.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    crate::training::train_and_regroup(&db).map_err(|e| format!("{:#}", e))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::ModelTrained,
        )
    }

    /// After labelling: the counts brought up to date, and the model
    /// retrained if there are new labels.
    fn labelling_ended(&mut self) -> Task<Message> {
        let before = self.ollama_labels;
        self.ollama_labels = self.db.pair_label_counts(crate::training::SOURCE_OLLAMA).unwrap_or((0, 0));
        if self.ollama_labels != before { self.train_model() } else { Task::none() }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let previous_status = self.status_message.clone();
        let task = self.handle_message(message);

        // Notifications off: every message is dropped here, wherever it came from
        if !self.notifications {
            self.status_message = None;
        }

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
                let other_story = self.selected_article_id != Some(article_id) || self.reader_group.is_some();
                self.reader_group = None;
                self.selected_article_id = Some(article_id);
                self.mark_opened_story_read(article_id);
                self.open_article = self.articles.iter().find(|a| a.id == article_id).cloned();
                // The list holds no story bodies; load just this one
                let blocks = self
                    .db
                    .get_article(article_id)
                    .ok()
                    .flatten()
                    .map(|article| reader_blocks(&article))
                    .unwrap_or_default();
                let had_images = !self.story_images.is_empty();
                self.prune_story_images(&blocks);
                (self.reader_texts, self.reader_links) = blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text, links } => Some((
                            text_editor::Content::with_text(text),
                            std::sync::Arc::new(links.clone()),
                        )),
                        Block::Image { .. } => None,
                    })
                    .unzip();
                self.pressed_link = None;
                self.menu_link = None;
                // Like a browser on a new page: a different story closes the
                // find bar and its highlights. (Reopening the same story keeps
                // them, with the matches found again in its fresh text.)
                if other_story {
                    self.find = None;
                } else {
                    self.update_find_matches();
                }
                self.reader_text_menu = None;
                self.reader_body = Some((article_id, blocks));
                // A new story starts at its top (the reader's scroll position
                // otherwise carries over from the previous one)
                let to_top = then_redraw(scrollable::snap_to(reader_scroll_id(), scrollable::RelativeOffset::START));
                if had_images {
                    Task::batch([to_top, release_memory_soon()])
                } else {
                    to_top
                }
            }
            Message::ScrollReader(dy) => {
                // Not while a dialog is up: it covers the reader
                if self.settings.is_some() || self.add_feed.is_some() || self.editing_feed.is_some() {
                    return Task::none();
                }
                then_redraw(scrollable::scroll_by(reader_scroll_id(), scrollable::AbsoluteOffset { x: 0.0, y: dy }))
            }
            Message::SelectAdjacentStory(forward) => {
                // Not while a dialog or rename field is up: the arrows belong there
                if self.settings.is_some()
                    || self.add_feed.is_some()
                    || self.editing_feed.is_some()
                    || self.renaming_feed.is_some()
                    || self.renaming_folder.is_some()
                    || self.articles.is_empty()
                {
                    return Task::none();
                }
                if self.grouped {
                    return self.select_adjacent_group(forward);
                }
                let position = |app: &Self| {
                    app.selected_article_id
                        .and_then(|id| app.articles.iter().position(|a| a.id == id))
                };
                // Stepping past the loaded part of the list loads more
                if forward
                    && position(self).is_some_and(|i| i + 1 == self.articles.len())
                    && self.articles.len() < self.article_total
                {
                    self.article_render_limit += ARTICLE_PAGE_SIZE;
                    self.reload_articles();
                }
                let current = position(self);
                let target = match (current, forward) {
                    // Nothing open yet: either arrow starts at the top
                    (None, _) => 0,
                    (Some(i), true) if i + 1 < self.articles.len() => i + 1,
                    (Some(i), false) if i > 0 => i - 1,
                    _ => return Task::none(), // already at the end
                };
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                let id = self.articles[target].id;
                let from = current.map(|i| self.articles[i].id);
                let select = self.update(Message::SelectArticle(id));
                // The whole list moves by one story, so the selection keeps
                // its place on screen and the stories after it stay in view
                let scroll = match from {
                    Some(from) => scroll_by_step(article_list_scroll_id(), story_card_id(from), story_card_id(id)),
                    None => scroll_into_view(article_list_scroll_id(), story_card_id(id)),
                };
                Task::batch([select, then_redraw(scroll)])
            }
            Message::LoadStoryImage(url) => {
                match self.story_images.get(&url) {
                    None => {
                        self.story_images.insert(url.clone(), ImageState::Loading);
                    }
                    Some(&ImageState::Released { width, height }) => {
                        self.story_images.insert(url.clone(), ImageState::Reloading { width, height });
                    }
                    Some(_) => return Task::none(), // already loaded or loading
                }
                self.image_queue.push_back(url);
                self.start_image_downloads()
            }
            Message::ReaderTextAction(index, action) => {
                // Read-only: allow selecting, moving, clicking, but never editing
                if !action.is_edit() {
                    let is_click = matches!(action, text_editor::Action::Click(_));
                    if let Some(content) = self.reader_texts.get_mut(index) {
                        content.perform(action);
                    }
                    // A click on a link opens it on release; any other
                    // action (a drag to select, keys) cancels that
                    self.pressed_link = if is_click { self.link_at_cursor(index) } else { None };
                }
                Task::none()
            }
            Message::ReaderMouseReleased => {
                if let Some(url) = self.pressed_link.take() {
                    if let Err(e) = open_in_browser(&url) {
                        self.status_message = Some((e, true));
                    }
                }
                Task::none()
            }
            Message::OpenReaderTextMenu(index, position) => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.reader_text_menu = Some(index);
                self.pressed_link = None;
                // Whether the right click is on a link. Finding out moves the
                // text cursor there, so not while text is selected (the menu's
                // Copy is for the selection then).
                self.menu_link = None;
                if let Some(content) = self.reader_texts.get_mut(index) {
                    if content.selection().is_none() {
                        content.perform(text_editor::Action::Click(position));
                        self.menu_link = self.link_at_cursor(index);
                    }
                }
                Task::none()
            }
            Message::CopyLink(url) => {
                self.reader_text_menu = None;
                iced::clipboard::write(url)
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
                self.image_downloads.remove(&url);
                let next = self.start_image_downloads();
                // Ignore downloads for a story that's no longer open
                if let Some(state) = self.story_images.get_mut(&url) {
                    self.image_loads += 1;
                    *state = match result {
                        Ok(StoryImage::Raster { width, height, pixels }) => ImageState::Raster {
                            handle: iced::widget::image::Handle::from_rgba(width, height, pixels),
                            width,
                            height,
                            order: self.image_loads,
                        },
                        Err(e) => {
                            tracing::debug!("Story image {} failed: {}", url, e);
                            ImageState::Failed
                        }
                    };
                    if self.release_story_images_over_budget(&url) {
                        return Task::batch([next, release_memory_soon()]);
                    }
                }
                next
            }
            Message::FeedAdded(result) => {
                self.syncs_running = self.syncs_running.saturating_sub(1);
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
                // New stories: group them in the background
                self.group_stories()
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
            Message::OpenSettings => {
                self.ollama_labels = self.db.pair_label_counts(crate::training::SOURCE_OLLAMA).unwrap_or((0, 0));
                if let (EmbeddingStatus::Idle, Ok(count)) = (&self.embeddings, self.db.embedding_count()) {
                    if count > 0 {
                        self.embeddings = EmbeddingStatus::Ready(count);
                    }
                }
                let ollama = self.ollama.clone();
                let check = Task::perform(async move { ollama.models().await }, Message::OllamaModels);
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.reader_text_menu = None;
                self.settings = Some(SettingsState {
                    keep_starred: true,
                    confirm_clear: None,
                    choosing_file: false,
                    import_result: None,
                    export_result: None,
                });
                check
            }
            Message::OllamaModels(models) => {
                // The saved model, if it's installed; otherwise the default, or any
                if let Ok(installed) = &models {
                    if !installed.contains(&self.ollama_model) {
                        let fallback = if installed.iter().any(|m| m == crate::ollama::DEFAULT_MODEL) {
                            Some(crate::ollama::DEFAULT_MODEL.to_string())
                        } else {
                            installed.first().cloned()
                        };
                        if let Some(model) = fallback {
                            self.ollama_model = model;
                        }
                    }
                }
                self.ollama_models = Some(models);
                Task::none()
            }
            Message::SetOllamaModel(model) => {
                let _ = self.db.set_setting(OLLAMA_MODEL_SETTING, &model);
                self.ollama_model = model;
                Task::none()
            }
            Message::StartLabelling => {
                if self.labelling.is_some() {
                    return Task::none();
                }
                let db = self.db.clone();
                let fetcher = self.fetcher.clone();
                let dir = crate::embeddings::model_dir(db.path());
                let embed_db = db.clone();
                // Recent stories need embeddings first (and the model, the first time)
                let prepare = move || async move {
                    crate::embeddings::ensure_model(&dir, |url, max| {
                        let fetcher = fetcher.clone();
                        async move { fetcher.download(url, max).await }
                    })
                    .await?;
                    tokio::task::spawn_blocking(move || {
                        crate::embeddings::embed_new_stories(&embed_db, &dir).map_err(|e| format!("{:#}", e))
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                    Ok(())
                };
                let (ollama, model) = (self.ollama.clone(), self.ollama_model.clone());
                let events = iced::stream::channel(100, move |sender| {
                    crate::training::label_with_ollama(db, ollama, model, prepare, sender)
                });
                let (task, handle) = Task::run(events, Message::Labelling).abortable();
                self.labelling = Some(Labelling {
                    status: crate::training::LabelEvent::Preparing,
                    _handle: handle.abort_on_drop(),
                });
                task
            }
            Message::StopLabelling => {
                self.labelling = None; // dropping the handle stops it
                self.labelling_ended()
            }
            Message::ModelTrained(result) => {
                self.training_model = false;
                match result {
                    Ok(Some(info)) => {
                        tracing::info!("Grouping model trained on {} examples", info.examples);
                        self.model_info = Some(info);
                        if self.grouped {
                            self.reload_articles();
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Training the grouping model failed: {}", e);
                        self.status_message = Some((format!("Couldn't learn from the examples: {}", e), true));
                    }
                }
                // Stories that arrived meanwhile
                self.group_stories()
            }
            Message::Labelling(event) => {
                use crate::training::LabelEvent;
                if let LabelEvent::Progress { .. } = &event {
                    if self.embeddings == EmbeddingStatus::Working || self.embeddings == EmbeddingStatus::Idle {
                        self.embeddings = EmbeddingStatus::Ready(self.db.embedding_count().unwrap_or(0));
                    }
                }
                if let LabelEvent::Finished(result) = &event {
                    self.labelling = None;
                    let train = self.labelling_ended();
                    self.status_message = Some(match result {
                        Ok(n) => (
                            format!("Ollama labelled {} {} of stories.", n, if *n == 1 { "pair" } else { "pairs" }),
                            false,
                        ),
                        Err(e) => (format!("Labelling stopped: {}", e), true),
                    });
                    return train;
                }
                if let Some(labelling) = &mut self.labelling {
                    labelling.status = event;
                }
                Task::none()
            }
            Message::CloseSettings => {
                self.settings = None;
                Task::none()
            }
            Message::SetKeepStarred(keep) => {
                if let Some(settings) = &mut self.settings {
                    settings.keep_starred = keep;
                }
                Task::none()
            }
            Message::AskClearStories => {
                let total = self.stats.1;
                let starred = self.db.starred_count().unwrap_or(0);
                if let Some(settings) = &mut self.settings {
                    let to_delete = if settings.keep_starred { total - starred } else { total };
                    settings.confirm_clear = Some((to_delete.max(0), starred));
                }
                Task::none()
            }
            Message::CancelClearStories => {
                if let Some(settings) = &mut self.settings {
                    settings.confirm_clear = None;
                }
                Task::none()
            }
            Message::ClearStories => {
                // A refresh in progress would immediately add stories back
                if self.is_syncing() {
                    return Task::none();
                }
                let keep_starred = self.settings.as_ref().is_none_or(|s| s.keep_starred);
                self.settings = None;
                match self.db.clear_stories(keep_starred) {
                    Ok(deleted) => {
                        self.status_message = Some((
                            format!(
                                "Deleted {} archived {}",
                                deleted,
                                if deleted == 1 { "story" } else { "stories" }
                            ),
                            false,
                        ));
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Failed to clear stories: {}", e), true));
                    }
                }
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_data();
                self.close_story_if_deleted()
            }
            Message::ImportOpml => {
                if let Some(settings) = &mut self.settings {
                    settings.choosing_file = true;
                    settings.import_result = None;
                }
                Task::perform(
                    async {
                        let file = rfd::AsyncFileDialog::new()
                            .set_title("Import feeds from OPML")
                            .add_filter("OPML files", &["opml", "xml"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                            .await?;
                        let name = file.file_name();
                        let contents = String::from_utf8(file.read().await)
                            .map_err(|_| format!("{} isn't a text file", name));
                        Some(contents.map(|contents| (name, contents)))
                    },
                    Message::OpmlFilePicked,
                )
            }
            Message::OpmlFilePicked(picked) => {
                if let Some(settings) = &mut self.settings {
                    settings.choosing_file = false;
                }
                let Some(picked) = picked else {
                    return Task::none(); // cancelled
                };
                let mut fetch = Task::none();
                let outcome = match picked {
                    Err(e) => (e, true),
                    Ok((name, contents)) => match opml::import_new_feeds(&contents, &self.db) {
                        Err(e) => (format!("Couldn't import {}: {}", name, e), true),
                        Ok(import) => {
                            self.reload_data();
                            // Show where the new feeds went
                            self.expanded_folders.extend(self.folders.iter().map(|f| f.id));
                            self.save_collapsed_folders();
                            let added = import.added.len();
                            let plural = |n: usize, one: &str, many: &str| {
                                format!("{} {}", n, if n == 1 { one } else { many })
                            };
                            let mut message = if added == 0 && import.skipped == 0 {
                                format!("No feeds found in {}.", name)
                            } else if added == 0 {
                                format!(
                                    "Nothing new in {}: you're already subscribed to all {}.",
                                    name,
                                    plural(import.skipped, "feed", "feeds")
                                )
                            } else {
                                let mut m = format!("Added {}", plural(added, "feed", "feeds"));
                                if import.folders_created > 0 {
                                    m += &format!(
                                        " and {}",
                                        plural(import.folders_created, "folder", "folders")
                                    );
                                }
                                m += &format!(" from {}.", name);
                                if import.skipped > 0 {
                                    m += &format!(
                                        " Skipped {} you already have.",
                                        plural(import.skipped, "feed", "feeds")
                                    );
                                }
                                m
                            };
                            if added > 0 {
                                if self.is_syncing() {
                                    message += " Their stories will download on the next refresh.";
                                } else {
                                    message += " Downloading their stories…";
                                    fetch = self.refresh_feeds("the imported feeds".to_string(), import.added);
                                }
                            }
                            (message, false)
                        }
                    },
                };
                match &mut self.settings {
                    Some(settings) => settings.import_result = Some(outcome),
                    // Settings closed while the file chooser was open
                    None => self.status_message = Some(outcome),
                }
                fetch
            }
            Message::ExportOpml => {
                // Snapshot the subscriptions now, so the file matches what was
                // on screen when the button was pressed
                let (opml_text, feed_count) = match opml::export_opml_from_db(&self.db) {
                    Ok(text) => (text, self.feeds.len()),
                    Err(e) => {
                        if let Some(settings) = &mut self.settings {
                            settings.export_result = Some((format!("Couldn't export: {}", e), true));
                        }
                        return Task::none();
                    }
                };
                if let Some(settings) = &mut self.settings {
                    settings.choosing_file = true;
                    settings.export_result = None;
                }
                let file_name = format!(
                    "shreader-feeds-{}.opml",
                    chrono::Local::now().format("%Y-%m-%d")
                );
                Task::perform(
                    async move {
                        let file = rfd::AsyncFileDialog::new()
                            .set_title("Export feeds to OPML")
                            .set_file_name(&file_name)
                            .add_filter("OPML files", &["opml"])
                            .save_file()
                            .await?;
                        let name = file.file_name();
                        Some(
                            file.write(opml_text.as_bytes())
                                .await
                                .map(|()| (name.clone(), feed_count))
                                .map_err(|e| format!("Couldn't save {}: {}", name, e)),
                        )
                    },
                    Message::OpmlExported,
                )
            }
            Message::OpmlExported(saved) => {
                if let Some(settings) = &mut self.settings {
                    settings.choosing_file = false;
                }
                let Some(saved) = saved else {
                    return Task::none(); // cancelled
                };
                let outcome = match saved {
                    Ok((name, feeds)) => (
                        format!(
                            "Saved {} {} to {}.",
                            feeds,
                            if feeds == 1 { "feed" } else { "feeds" },
                            name
                        ),
                        false,
                    ),
                    Err(e) => (e, true),
                };
                match &mut self.settings {
                    Some(settings) => settings.export_result = Some(outcome),
                    None => self.status_message = Some(outcome),
                }
                Task::none()
            }
            Message::ReleaseMemory => {
                release_memory();
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
                if self.is_syncing() {
                    return Task::none();
                }
                self.syncs_running += 1;
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
            Message::AutoRefreshTick => {
                // Skipped if a refresh is already running; the next tick catches up
                if self.feeds.is_empty() {
                    return Task::none();
                }
                self.update(Message::RefreshClicked)
            }
            Message::OpenFind => {
                if self.reader_body.is_none() && self.reader_group.is_none() {
                    return Task::none(); // no story open
                }
                if self.find.is_none() {
                    self.find = Some(FindState::default());
                }
                let id = text_input::Id::new(FIND_INPUT_ID);
                Task::batch([text_input::focus(id.clone()), text_input::select_all(id)])
            }
            Message::FindQueryChanged(query) => {
                if let Some(find) = &mut self.find {
                    find.query = query;
                }
                self.update_find_matches();
                self.scroll_to_find_match()
            }
            Message::FindNext | Message::FindPrevious => {
                let forward = matches!(message, Message::FindNext);
                let Some(find) = &mut self.find else {
                    return Task::none();
                };
                let total = find.total();
                if total == 0 {
                    return Task::none();
                }
                find.current = if forward { (find.current + 1) % total } else { (find.current + total - 1) % total };
                self.scroll_to_find_match()
            }
            Message::CloseFind => {
                self.find = None;
                Task::none()
            }
            Message::WindowCloseRequested => {
                // With a tray to come back from: close just the window, and
                // keep running (and refreshing). Otherwise, quit.
                if !(self.tray && self.keep_in_tray) {
                    return iced::exit();
                }
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.reader_text_menu = None;
                match self.main_window.take() {
                    Some(id) => iced::window::close(id),
                    None => Task::none(),
                }
            }
            Message::Desktop(crate::desktop::Command::Show) => match self.main_window {
                Some(id) => iced::window::gain_focus(id),
                None => self.open_main_window(),
            },
            Message::Desktop(crate::desktop::Command::Refresh) => self.update(Message::RefreshClicked),
            Message::Desktop(crate::desktop::Command::Quit) | Message::Quit => iced::exit(),
            Message::SetKeepInTray(on) => {
                self.keep_in_tray = on;
                let _ = self.db.set_setting(KEEP_IN_TRAY_SETTING, if on { "true" } else { "false" });
                Task::none()
            }
            Message::ToggleGrouped => {
                self.grouped = !self.grouped;
                let _ = self.db.set_setting(GROUPED_SETTING, if self.grouped { "true" } else { "false" });
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_articles();
                let embed = self.embed_stories();
                Task::batch([
                    then_redraw(scrollable::snap_to(article_list_scroll_id(), scrollable::RelativeOffset::START)),
                    embed,
                ])
            }
            Message::StoriesEmbedded(result) => {
                self.embeddings = match result {
                    Ok(n) => {
                        if n > 0 {
                            tracing::info!("Embedded {} stories", n);
                        }
                        EmbeddingStatus::Ready(self.db.embedding_count().unwrap_or(0))
                    }
                    Err(e) => {
                        tracing::warn!("Story embeddings failed: {}", e);
                        EmbeddingStatus::Failed(e)
                    }
                };
                Task::none()
            }
            Message::StoriesGrouped(result) => {
                match result {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!("Grouped {} stories", n);
                        if self.grouped {
                            self.reload_articles();
                        }
                    }
                    Err(e) => tracing::warn!("Grouping stories failed: {}", e),
                }
                Task::none()
            }
            Message::SelectGroup(group) => {
                let Some(entry) = self.story_groups.iter().find(|g| g.group == group).cloned() else {
                    return Task::none();
                };
                // Opening the group reads all its stories
                for member in entry.members.iter().filter(|m| !m.is_read) {
                    self.mark_story_read(member);
                }
                // Oldest first: the story as it developed
                let stories: Vec<(Article, Vec<Block>)> = entry
                    .members
                    .iter()
                    .rev()
                    .filter_map(|member| self.db.get_article(member.id).ok().flatten())
                    .map(|article| {
                        let blocks = reader_blocks(&article);
                        (Article { summary: None, content_html: None, content_text: None, ..article }, blocks)
                    })
                    .collect();
                let all_blocks: Vec<Block> = stories.iter().flat_map(|(_, blocks)| blocks.iter().cloned()).collect();
                let had_images = !self.story_images.is_empty();
                self.prune_story_images(&all_blocks);
                (self.reader_texts, self.reader_links) = all_blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text, links } => Some((
                            text_editor::Content::with_text(text),
                            std::sync::Arc::new(links.clone()),
                        )),
                        Block::Image { .. } => None,
                    })
                    .unzip();
                if self.reader_group.as_ref().is_none_or(|g| g.group != group) {
                    self.find = None;
                }
                self.selected_article_id = None;
                self.open_article = None;
                self.reader_body = None;
                self.pressed_link = None;
                self.menu_link = None;
                self.reader_text_menu = None;
                self.reader_group = Some(ReaderGroup { group, title: entry.title().to_string(), stories });
                self.update_find_matches();
                let to_top = then_redraw(scrollable::snap_to(reader_scroll_id(), scrollable::RelativeOffset::START));
                let downloads = self.start_image_downloads();
                if had_images {
                    Task::batch([to_top, downloads, release_memory_soon()])
                } else {
                    Task::batch([to_top, downloads])
                }
            }
            Message::ToggleUnreadFirst => {
                self.unread_first = !self.unread_first;
                let _ = self
                    .db
                    .set_setting(UNREAD_FIRST_SETTING, if self.unread_first { "true" } else { "false" });
                // A different order: start again from the top
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_articles();
                then_redraw(scrollable::snap_to(article_list_scroll_id(), scrollable::RelativeOffset::START))
            }
            Message::SetNotifications(on) => {
                self.notifications = on;
                let _ = self.db.set_setting(NOTIFICATIONS_SETTING, if on { "true" } else { "false" });
                Task::none()
            }
            Message::SetLoadImages(on) => {
                self.load_images = on;
                let _ = self.db.set_setting(LOAD_IMAGES_SETTING, if on { "true" } else { "false" });
                Task::none()
            }
            Message::SetAutoRefresh(on) => {
                self.auto_refresh = on;
                let _ = self.db.set_setting(AUTO_REFRESH_SETTING, if on { "true" } else { "false" });
                Task::none()
            }
            Message::SetAutoRefreshInterval(RefreshInterval(minutes)) => {
                self.auto_refresh_minutes = minutes;
                let _ = self.db.set_setting(AUTO_REFRESH_MINUTES_SETTING, &minutes.to_string());
                Task::none()
            }
            Message::SyncFinished(result) => {
                self.syncs_running = self.syncs_running.saturating_sub(1);
                match result {
                    Ok(summaries) => {
                        let total_inserted: usize = summaries.iter().map(|s| s.articles_inserted).sum();
                        let total_updated: usize = summaries.iter().map(|s| s.articles_updated).sum();
                        let mut failed: Vec<&str> = summaries
                            .iter()
                            .filter(|s| s.error.is_some())
                            .map(|s| s.feed_title.as_str())
                            .collect();
                        failed.sort_unstable_by_key(|t| t.to_lowercase());
                        let mut message = format!(
                            "Sync complete: {} new {} archived, {} updated.",
                            total_inserted,
                            if total_inserted == 1 { "story" } else { "stories" },
                            total_updated
                        );
                        if !failed.is_empty() {
                            const NAMED: usize = 5;
                            let mut names = failed.iter().take(NAMED).copied().collect::<Vec<_>>().join(", ");
                            if failed.len() > NAMED {
                                names += &format!(" and {} more", failed.len() - NAMED);
                            }
                            message += &format!(
                                " {} feed{} couldn't be refreshed: {}.",
                                failed.len(),
                                if failed.len() == 1 { "" } else { "s" },
                                names
                            );
                        }
                        message += &held_back_note(&summaries);
                        self.status_message = Some((message, !failed.is_empty()));
                        self.reload_data();
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Sync failed: {}", e), true));
                    }
                }
                // New stories: group them in the background
                self.group_stories()
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
                self.syncs_running = self.syncs_running.saturating_sub(1);
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
                        let held_back = held_back_note(&summaries);
                        match (failed.len(), summaries.len()) {
                            (0, _) => (format!("Refreshed {}: {}.{}", name, new_stories, held_back), false),
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
                                    "Refreshed {}: {}. {} feed{} failed: {}.{}",
                                    name,
                                    new_stories,
                                    n,
                                    if n == 1 { "" } else { "s" },
                                    failed.join(", "),
                                    held_back
                                ),
                                true,
                            ),
                        }
                    }
                    Err(e) => (format!("Couldn't refresh {}: {}", name, e), true),
                });
                // New stories: group them in the background
                self.group_stories()
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
                self.close_story_if_deleted()
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
                self.reader_text_menu = None;
                if let Err(e) = open_in_browser(&url) {
                    self.status_message = Some((e, true));
                }
                Task::none()
            }
            Message::ShowMoreArticles => {
                self.article_render_limit += ARTICLE_PAGE_SIZE;
                self.reload_articles();
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
                self.save_collapsed_folders();
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
            Message::DragFolderStarted(folder_id) => {
                self.feed_menu = None;
                self.folder_menu = None;
                self.article_menu = None;
                self.dragging_folder = Some(folder_id);
                self.drag_hover_target = None;
                Task::none()
            }
            Message::DragOverTarget(target) => {
                if self.dragging_feed.is_some() || self.dragging_folder.is_some() {
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

    /// Closes the open story if it no longer exists (its feed was deleted or
    /// the archive cleared), rather than leaving it on screen with buttons
    /// that no longer do anything.
    fn close_story_if_deleted(&mut self) -> Task<Message> {
        let Some(id) = self.selected_article_id else {
            return Task::none();
        };
        if matches!(self.db.get_article(id), Ok(Some(_))) {
            return Task::none();
        }
        self.selected_article_id = None;
        self.open_article = None;
        self.reader_body = None;
        self.reader_texts.clear();
        self.reader_text_menu = None;
        if self.article_menu == Some(id) {
            self.article_menu = None;
        }
        self.story_images.clear();
        self.image_queue.clear();
        self.image_downloads.clear();
        release_memory_soon()
    }

    /// Whether a refresh or a new feed's download is running.
    fn is_syncing(&self) -> bool {
        self.syncs_running > 0
    }

    fn finish_drag(&mut self, target: Option<DropTarget>) {
        if let (Some(feed_id), Some(target)) = (self.dragging_feed, target) {
            self.drop_feed(feed_id, target);
        }
        if let (Some(folder_id), Some(DropTarget::Folder(Some(target_id)))) = (self.dragging_folder, target) {
            self.drop_folder(folder_id, target_id);
        }
        self.dragging_feed = None;
        self.dragging_folder = None;
        self.drag_hover_target = None;
    }

    /// Adds a feed and fetches its stories in the background (reports via
    /// `FeedAdded`). With no `title`, the URL stands in until the first sync
    /// replaces it with the feed's own title.
    fn subscribe(&mut self, url: String, title: Option<String>, folder_id: Option<i64>) -> Task<Message> {
        self.syncs_running += 1;
        let db = self.db.clone();
        let fetcher = self.fetcher.clone();

        Task::perform(
            async move {
                let title = title.unwrap_or_else(|| url.clone());
                let feed_id = db
                    .add_or_update_feed(&url, &title, None, None, folder_id)
                    .map_err(|e| format!("Database error: {}", e))?;

                let summary = fetcher.fetch_and_sync(&db, feed_id).await;
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
        if self.is_syncing() {
            self.status_message =
                Some(("A refresh is already running. Try again when it finishes.".to_string(), true));
            return Task::none();
        }
        if feed_ids.is_empty() {
            return Task::none();
        }
        self.syncs_running += 1;
        let db = self.db.clone();
        let fetcher = self.fetcher.clone();
        Task::perform(
            async move { Ok(fetcher.sync_feeds(&db, feed_ids).await) },
            move |result| Message::PartialRefreshFinished(name.clone(), result),
        )
    }

    /// Marks a story read when it's opened. Updates the counts in memory
    /// instead of reloading everything from the database, which took ~100 ms
    /// per click with a few thousand stories.
    /// Marks a story read wherever it is (the open group's stories needn't be
    /// in the loaded part of the list), keeping the counts right.
    fn mark_story_read(&mut self, story: &Article) {
        if let Err(e) = self.db.set_article_read(story.id, true) {
            self.status_message = Some((format!("Failed to mark story read: {}", e), true));
            return;
        }
        for article in self.articles.iter_mut().chain(self.story_groups.iter_mut().flat_map(|g| g.members.iter_mut())) {
            if article.id == story.id {
                article.is_read = true;
            }
        }
        self.stats.2 = (self.stats.2 - 1).max(0);
        if let Some(feed) = self.feeds.iter_mut().find(|f| f.id == story.feed_id) {
            feed.unread_count = (feed.unread_count - 1).max(0);
            if let Some(folder_id) = feed.folder_id {
                if let Some(folder) = self.folders.iter_mut().find(|f| f.id == folder_id) {
                    folder.unread_count = (folder.unread_count - 1).max(0);
                }
            }
        }
    }

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
        // The grouped list holds its own copies
        for member in self.story_groups.iter_mut().flat_map(|g| g.members.iter_mut()) {
            if member.id == article_id {
                member.is_read = true;
            }
        }

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
                Block::Text { .. } => None,
            })
            .collect();
        // Keep what this story still uses (e.g. reopening the same story); retry failures
        self.story_images
            .retain(|url, state| urls.contains(url.as_str()) && !matches!(state, ImageState::Failed));
        // Downloads for images no longer shown are cancelled (dropping the
        // handle aborts them), and ones not yet started are forgotten
        self.image_downloads.retain(|url, _| self.story_images.contains_key(url));
        self.image_queue.retain(|url| self.story_images.contains_key(url));
    }

    /// Finds the find bar's query in the open story, starting again from the
    /// first match.
    fn update_find_matches(&mut self) {
        let Some(find) = &mut self.find else { return };
        let mut limit = MAX_FIND_MATCHES;
        // The open story's blocks, or every story's in an open group
        let blocks: Vec<&Block> = match (&self.reader_group, &self.reader_body) {
            (Some(group), _) => group.stories.iter().flat_map(|(_, blocks)| blocks).collect(),
            (None, Some((_, blocks))) => blocks.iter().collect(),
            (None, None) => Vec::new(),
        };
        find.matches = blocks
            .into_iter()
            .filter_map(|block| match block {
                Block::Text { text, .. } => {
                    let found = find_matches(text, find.query.trim(), limit);
                    limit -= found.len();
                    Some(found)
                }
                Block::Image { .. } => None,
            })
            .collect();
        find.current = 0;
    }

    /// Scrolls the story so the current find match is in view.
    fn scroll_to_find_match(&self) -> Task<Message> {
        if self.find.as_ref().is_some_and(|f| f.total() > 0) {
            then_redraw(scroll_into_view(reader_scroll_id(), find_match_id()))
        } else {
            Task::none()
        }
    }

    /// The find bar shown above the story.
    fn find_bar<'a>(&'a self, find: &'a FindState) -> Element<'a, Message> {
        let total = find.total();
        let count = if find.query.trim().is_empty() {
            String::new()
        } else if total == 0 {
            "No matches".to_string()
        } else {
            let more = if total >= MAX_FIND_MATCHES { "+" } else { "" };
            format!("{} of {}{}", find.current + 1, total, more)
        };
        let nav = |label: &'static str, message: Message| {
            button(text(label).size(12))
                .padding([4, 8])
                .style(button::secondary)
                .on_press_maybe((total > 0).then_some(message))
        };
        row![
            text_input("Find in story", &find.query)
                .id(text_input::Id::new(FIND_INPUT_ID))
                .on_input(Message::FindQueryChanged)
                .on_submit(Message::FindNext)
                .size(13)
                .padding(5)
                .width(Length::Fill),
            text(count).size(12).style(text::secondary),
            nav("Previous", Message::FindPrevious),
            nav("Next", Message::FindNext),
            button(text("Close").size(12))
                .padding([4, 8])
                .style(button::text)
                .on_press(Message::CloseFind),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    /// The link at the text cursor in story text block `index`, if any.
    fn link_at_cursor(&self, index: usize) -> Option<String> {
        let content = self.reader_texts.get(index)?;
        let (line, column) = content.cursor_position();
        let line_len = content.line(line).map_or(0, |text| text.len());
        self.reader_links.get(index)?.iter().find_map(|link| {
            // The cursor lands before or after the clicked character,
            // whichever half was clicked. Right after a link's last character
            // still counts, unless that's the end of the line (a click in
            // the empty space beyond it).
            let inside = link.line == line
                && link.start <= column
                && (column < link.end || (column == link.end && link.end < line_len));
            inside.then(|| link.url.clone())
        })
    }

    /// Starts queued image downloads, up to `MAX_IMAGE_DOWNLOADS` at once.
    fn start_image_downloads(&mut self) -> Task<Message> {
        let mut started = Vec::new();
        while self.image_downloads.len() < MAX_IMAGE_DOWNLOADS {
            let Some(url) = self.image_queue.pop_front() else { break };
            let fetcher = self.fetcher.clone();
            let (task, handle) = Task::perform(
                {
                    let url = url.clone();
                    async move { fetcher.fetch_image(&url).await }
                },
                {
                    let url = url.clone();
                    move |result| Message::StoryImageLoaded(url.clone(), result)
                },
            )
            .abortable();
            self.image_downloads.insert(url, handle.abort_on_drop());
            started.push(task);
        }
        Task::batch(started)
    }

    /// Keeps the open story's decoded images within `STORY_IMAGE_MEMORY` by
    /// releasing the ones loaded longest ago (never `keep`, the one that just
    /// arrived). Returns whether any were released.
    fn release_story_images_over_budget(&mut self, keep: &str) -> bool {
        let bytes = |width: u32, height: u32| u64::from(width) * u64::from(height) * 4;
        let mut used: u64 = self
            .story_images
            .values()
            .map(|state| match state {
                ImageState::Raster { width, height, .. } => bytes(*width, *height),
                _ => 0,
            })
            .sum();
        let mut released = false;
        while used > STORY_IMAGE_MEMORY {
            let oldest = self
                .story_images
                .iter()
                .filter_map(|(url, state)| match state {
                    ImageState::Raster { order, .. } if url != keep => Some((*order, url.clone())),
                    _ => None,
                })
                .min();
            let Some((_, url)) = oldest else { break };
            if let Some(state) = self.story_images.get_mut(&url) {
                if let ImageState::Raster { width, height, .. } = *state {
                    used -= bytes(width, height);
                    *state = ImageState::Released { width, height };
                    released = true;
                }
            }
        }
        released
    }

    /// The story body: text paragraphs with the story's images in between.
    ///
    /// With `selectable`, text blocks are the read-only editors in
    /// `reader_texts` (one per text block, in order) so they can be copied.
    /// `first_text` is the index in `reader_texts` of the first text block
    /// (an open group's stories share one list).
    fn story_body<'a>(&'a self, blocks: &'a [Block], selectable: bool, first_text: usize) -> Element<'a, Message> {
        let mut body = column![].spacing(14).width(Length::Fill);
        let mut text_index = first_text;
        for block in blocks {
            body = body.push(match block {
                Block::Text { text: body_text, links } => {
                    let index = text_index;
                    text_index += 1;
                    match self.reader_texts.get(index).filter(|_| selectable) {
                        Some(content) => self.selectable_text(index, content, body_text, links),
                        None => text(body_text.clone()).size(14).width(Length::Fill).into(),
                    }
                }
                Block::Image { src } => self.story_image(src),
            });
        }
        body.into()
    }

    /// A story's card in the story list.
    fn story_list_card<'a>(&'a self, article: &'a Article) -> Element<'a, Message> {
        let is_selected = self.selected_article_id == Some(article.id);

        let feed_name = article.feed_title.as_deref().unwrap_or("RSS");
        let date_str = article
            .shown_date()
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
            .color(if article.is_read { READ_TITLE } else { UNREAD_TITLE });

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
            .style(move |theme, status| story_card(theme, status, is_selected))
            .on_press(Message::SelectArticle(article.id));

        let menu = (self.article_menu == Some(article.id)).then(|| article_menu_view(article));
        let card = container(
            context_menu(item_btn)
                .menu(menu)
                .on_right_press(Message::OpenArticleMenu(article.id))
                .on_dismiss(Message::CloseArticleMenu),
        );
        // So the arrow keys can find the cards to scroll by
        let card = card.id(story_card_id(article.id));
        card.into()
    }

    /// A group's card in the grouped story list: its first report's
    /// headline (green while any of its stories are unread), how many
    /// stories, from where, and the latest one's time.
    fn group_list_card<'a>(&'a self, entry: &'a StoryGroupEntry) -> Element<'a, Message> {
        let is_selected = self.reader_group.as_ref().is_some_and(|g| g.group == entry.group);
        let mut sources: Vec<&str> = Vec::new();
        for member in &entry.members {
            let name = member.feed_title.as_deref().unwrap_or("RSS");
            if !sources.contains(&name) {
                sources.push(name);
            }
        }
        const NAMED: usize = 2;
        let mut from = sources.iter().take(NAMED).copied().collect::<Vec<_>>().join(", ");
        if sources.len() > NAMED {
            from += &format!(" +{}", sources.len() - NAMED);
        }
        let latest = entry
            .members
            .first()
            .and_then(Article::shown_date)
            .map(format_date)
            .unwrap_or_default();
        let unread = entry.unread();
        let count = if unread > 0 && unread < entry.members.len() {
            format!("{} stories, {} unread", entry.members.len(), unread)
        } else {
            format!("{} stories", entry.members.len())
        };
        let content = column![
            text(entry.title()).size(13).color(if unread > 0 { UNREAD_TITLE } else { READ_TITLE }),
            row![
                text(count).size(11).style(text::secondary),
                text("•").size(11).style(text::secondary),
                text(from).size(11).style(text::secondary),
                text("•").size(11).style(text::secondary),
                text(latest).size(11).style(text::secondary),
            ]
            .spacing(4)
            .wrap(),
        ]
        .spacing(3)
        .width(Length::Fill);
        let card = button(row![icon(Icon::Stack, 14.0, Tint::Secondary), content].spacing(10).align_y(Alignment::Center))
            .width(Length::Fill)
            .padding(8)
            .style(move |theme, status| story_card(theme, status, is_selected))
            .on_press(Message::SelectGroup(entry.group));
        container(card).id(group_card_id(entry.group)).into()
    }

    /// A story text block that can be selected (drag, double/triple click,
    /// Ctrl+A) and copied (Ctrl+C or right click). Styled to look like plain text.
    fn selectable_text<'a>(
        &'a self,
        index: usize,
        content: &'a text_editor::Content,
        block_text: &'a str,
        block_links: &'a [LinkSpan],
    ) -> Element<'a, Message> {
        let links = self.reader_links.get(index).cloned().unwrap_or_default();
        let editor = text_editor(content)
            .highlight_with::<LinkHighlighter>(links, |_, _| iced::advanced::text::highlighter::Format {
                color: Some(LINK_COLOR),
                font: None,
            })
            .on_action(move |action| Message::ReaderTextAction(index, action))
            // Plain arrows switch stories (Left/Right) and scroll the story
            // (Up/Down) even after clicking into the text; Shift+arrows still
            // extend a selection
            .key_binding(|key_press| match key_press.key.as_ref() {
                iced::keyboard::Key::Named(
                    iced::keyboard::key::Named::ArrowLeft
                    | iced::keyboard::key::Named::ArrowRight
                    | iced::keyboard::key::Named::ArrowUp
                    | iced::keyboard::key::Named::ArrowDown,
                ) if key_press.modifiers.is_empty() => None,
                _ => text_editor::Binding::from_key_press(key_press),
            })
            .size(READER_TEXT_SIZE)
            .padding(0)
            .wrapping(READER_TEXT_WRAPPING)
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
            let mut items = column![].spacing(2);
            if let Some(url) = &self.menu_link {
                items = items
                    .push(menu_item("Open link", Some(Message::OpenUrl(url.clone()))))
                    .push(menu_item("Copy link address", Some(Message::CopyLink(url.clone()))))
                    .push(horizontal_rule(1));
            }
            items = items
                .push(menu_item("Copy", has_selection.then_some(Message::CopyReaderSelection(index))))
                .push(menu_item("Select all", Some(Message::SelectAllReaderText(index))));
            menu_card(items, 170.0)
        });

        // The hand pointer over links (the editor itself only has the I-beam),
        // and find highlights
        let mut editor = story_text(editor, block_text, block_links, READER_TEXT_SIZE, READER_TEXT_WRAPPING);
        if let Some(find) = &self.find {
            if let Some(matches) = find.matches.get(index) {
                let current = find
                    .current_location()
                    .and_then(|(block, local)| (block == index).then_some(local));
                editor = editor.matches(matches, current, find_match_id().into());
            }
        }

        context_menu(editor)
            .menu(menu)
            .focus_on_right_press()
            .on_right_press_at(move |position| Message::OpenReaderTextMenu(index, position))
            .on_left_release(Message::ReaderMouseReleased)
            .on_dismiss(Message::CloseReaderTextMenu)
            .into()
    }

    /// One story image, centered. Clicking it opens the full image in the browser.
    fn story_image<'a>(&'a self, src: &str) -> Element<'a, Message> {
        let open = Message::OpenUrl(src.to_string());
        let content: Element<'a, Message> = match self.story_images.get(src) {
            // Shrink-sized: small images stay at their own size, large ones are
            // scaled down to the column width (keeping their proportions)
            Some(ImageState::Raster { handle, .. }) => mouse_area(iced::widget::image(handle.clone()))
                .on_press(open)
                .interaction(mouse::Interaction::Pointer)
                .into(),
            // Released to save memory: same-sized space, reloaded when near
            Some(&ImageState::Released { width, height }) => on_visible(
                image_space(width, height),
                Message::LoadStoryImage(src.to_string()),
                IMAGE_PRELOAD_MARGIN,
            )
            .into(),
            Some(&ImageState::Reloading { width, height }) => image_space(width, height).into(),
            Some(ImageState::Loading) => {
                text("Loading image…").size(11).style(text::secondary).into()
            }
            // Automatic loading off: only on request (an image can tell its
            // sender that the story was opened)
            None if !self.load_images => button(text("Show image").size(11))
                .padding([2, 6])
                .style(button::text)
                .on_press(Message::LoadStoryImage(src.to_string()))
                .into(),
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

    /// Saves which folders are collapsed, so they open the same way next time.
    fn save_collapsed_folders(&self) {
        let collapsed: Vec<String> = self
            .folders
            .iter()
            .filter(|f| !self.expanded_folders.contains(&f.id))
            .map(|f| f.id.to_string())
            .collect();
        let _ = self.db.set_setting(COLLAPSED_FOLDERS_SETTING, &collapsed.join(","));
    }

    /// Moves a dragged folder into the place of the folder it was dropped on.
    fn drop_folder(&mut self, folder_id: i64, target_id: i64) {
        if folder_id == target_id {
            return;
        }
        let original: Vec<i64> = self.folders.iter().map(|f| f.id).collect();
        let Some(target_idx) = original.iter().position(|&id| id == target_id) else {
            return;
        };
        let mut order: Vec<i64> = original.iter().copied().filter(|&id| id != folder_id).collect();
        // Same rule as feeds: it lands after the target when moving down, before
        // it when moving up
        order.insert(target_idx.min(order.len()), folder_id);
        if let Err(e) = self.db.order_folders(&order) {
            self.status_message = Some((format!("Failed to move folder: {}", e), true));
        }
        self.reload_data();
    }

    /// Where to draw the insertion line on a folder header, if a dragged
    /// folder is hovering it.
    fn folder_insert_edge(&self, folder_id: i64) -> Option<InsertEdge> {
        let dragged_id = self.dragging_folder?;
        if dragged_id == folder_id || self.drag_hover_target != Some(DropTarget::Folder(Some(folder_id))) {
            return None;
        }
        let from = self.folders.iter().position(|f| f.id == dragged_id)?;
        let to = self.folders.iter().position(|f| f.id == folder_id)?;
        Some(if from < to { InsertEdge::Bottom } else { InsertEdge::Top })
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

    /// The Settings window: import/export feeds as OPML, clear archived stories
    /// (asks for confirmation first, inside the same window).
    fn settings_view<'a>(&'a self, settings: &'a SettingsState) -> Element<'a, Message> {
        let plural = |n: i64, one: &str, many: &str| {
            format!("{} {}", n, if n == 1 { one } else { many })
        };

        // Import from OPML
        let mut import_section = column![row![
            column![
                text("Import from OPML").size(14),
                text("Add feeds and folders exported from another reader. Feeds you already have are left as they are.")
                    .size(12)
                    .style(text::secondary),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text("Import…").size(13))
                .padding([5, 12])
                .style(button::secondary)
                .on_press_maybe((!settings.choosing_file).then_some(Message::ImportOpml)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)]
        .spacing(6);
        if let Some((message, is_error)) = &settings.import_result {
            import_section = import_section.push(
                text(message)
                    .size(12)
                    .style(if *is_error { text::danger } else { text::success }),
            );
        }

        // Automatic refresh
        let mut interval_choices: Vec<RefreshInterval> =
            AUTO_REFRESH_CHOICES.iter().copied().map(RefreshInterval).collect();
        if !AUTO_REFRESH_CHOICES.contains(&self.auto_refresh_minutes) {
            interval_choices.push(RefreshInterval(self.auto_refresh_minutes));
            interval_choices.sort_by_key(|i| i.0);
        }
        let refresh_section = column![
            text("Automatic refresh").size(14),
            row![
                checkbox("Refresh all feeds every", self.auto_refresh)
                    .on_toggle(Message::SetAutoRefresh)
                    .size(14)
                    .text_size(12),
                pick_list(
                    interval_choices,
                    Some(RefreshInterval(self.auto_refresh_minutes)),
                    Message::SetAutoRefreshInterval,
                )
                .text_size(12)
                .padding([3, 8]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text("Feeds are also refreshed when the app starts.")
                .size(12)
                .style(text::secondary),
        ]
        .spacing(6);

        // Images
        let images_section = column![
            text("Images").size(14),
            checkbox("Load images in stories automatically", self.load_images)
                .on_toggle(Message::SetLoadImages)
                .size(14)
                .text_size(12),
            text("Loading an image can let the sender see that you opened the story. When this is off, each image has a button to load it.")
                .size(12)
                .style(text::secondary),
        ]
        .spacing(6);

        // Story grouping
        let grouping_status = match &self.embeddings {
            EmbeddingStatus::Idle => "The language model that helps group stories isn't set up yet. It's downloaded (once, 91 MB) when you first use Grouped.".to_string(),
            EmbeddingStatus::Working => "Setting up: downloading the language model (the first time, 91 MB) and analysing recent stories…".to_string(),
            EmbeddingStatus::Ready(n) => format!("Language model ready: {} recent {} analysed.", n, if *n == 1 { "story" } else { "stories" }),
            EmbeddingStatus::Failed(e) => format!("The language model couldn't be set up: {}", e),
        };
        // Training examples from Ollama
        let ollama_row: Element<'_, Message> = match &self.ollama_models {
            None => text("Looking for Ollama…").size(12).style(text::secondary).into(),
            Some(Err(e)) => text(format!("{}. Install and start Ollama (ollama.com) to label examples.", e))
                .size(12)
                .style(text::secondary)
                .into(),
            Some(Ok(models)) if models.is_empty() => {
                text("Ollama has no models installed. Install one with: ollama pull llama3.1:8b")
                    .size(12)
                    .style(text::secondary)
                    .into()
            }
            Some(Ok(models)) => row![
                text("Ollama model").size(12),
                pick_list(models.as_slice(), Some(&self.ollama_model), Message::SetOllamaModel)
                    .text_size(12)
                    .padding([3, 8]),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        };
        let can_label = matches!(&self.ollama_models, Some(Ok(models)) if !models.is_empty());
        let label_row: Element<'_, Message> = match &self.labelling {
            Some(labelling) => {
                use crate::training::LabelEvent;
                let progress = match &labelling.status {
                    LabelEvent::Preparing => "Getting ready: analysing recent stories…".to_string(),
                    LabelEvent::Progress { done, total, same } => {
                        format!("Labelling: {} of {} pairs ({} the same story)", done, total, same)
                    }
                    LabelEvent::Finished(_) => String::new(),
                };
                row![
                    text(progress).size(12).width(Length::Fill),
                    button(text("Stop").size(12)).padding([4, 10]).style(button::secondary).on_press(Message::StopLabelling),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into()
            }
            None => row![
                text(format!(
                    "Examples from Ollama: {} pairs, {} the same story.",
                    self.ollama_labels.0, self.ollama_labels.1
                ))
                .size(12)
                .style(text::secondary)
                .width(Length::Fill),
                button(text(format!("Label the last {} days", crate::training::LABEL_DAYS)).size(12))
                    .padding([4, 10])
                    .style(button::secondary)
                    .on_press_maybe(can_label.then_some(Message::StartLabelling)),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        };
        let grouping_section = column![
            text("Story grouping").size(14),
            text(grouping_status)
                .size(12)
                .style(if matches!(self.embeddings, EmbeddingStatus::Failed(_)) { text::danger } else { text::secondary }),
            ollama_row,
            label_row,
            text(if self.training_model {
                "Learning from the examples and regrouping recent stories…".to_string()
            } else if let Some(info) = &self.model_info {
                format!(
                    "Grouping uses what it learned from {} examples. Tested on {} it hadn't seen, it agreed with {:.0}% of them.",
                    info.examples,
                    info.tested,
                    info.accuracy * 100.0
                )
            } else {
                format!(
                    "Grouping goes by shared words until there are at least {} examples to learn from.",
                    crate::training::MIN_EXAMPLES
                )
            })
            .size(12)
            .style(text::secondary),
            text("Ollama judges whether likely pairs of recent stories are the same news. Its answers are examples the grouping learns from, alongside your own corrections. A run continues where the last one stopped.")
                .size(12)
                .style(text::secondary),
        ]
        .spacing(6);

        // System tray
        let tray_section = column![
            text("System tray").size(14),
            // Shown as it actually behaves: without a tray, closing quits
            // whatever the saved choice
            checkbox("Keep running in the system tray when the window is closed", self.tray && self.keep_in_tray)
                .on_toggle_maybe(self.tray.then_some(Message::SetKeepInTray))
                .size(14)
                .text_size(12),
            text(if self.tray {
                "Feeds keep refreshing in the background. Click the tray icon to open the window again; quit from its menu, or with Ctrl+Q."
            } else {
                "This desktop doesn't have a system tray, so closing the window quits Shreader."
            })
            .size(12)
            .style(text::secondary),
        ]
        .spacing(6);

        // Notifications
        let notifications_section = column![
            text("Notifications").size(14),
            checkbox("Show notifications", self.notifications)
                .on_toggle(Message::SetNotifications)
                .size(14)
                .text_size(12),
            text("Messages in the corner of the window, after refreshes and other actions. When this is off, none appear, including ones about problems.")
                .size(12)
                .style(text::secondary),
        ]
        .spacing(6);

        // Export to OPML
        let mut export_section = column![row![
            column![
                text("Export to OPML").size(14),
                text("Save your feeds and folders to a file, as a backup or to use in another reader. Stories aren't included.")
                    .size(12)
                    .style(text::secondary),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text("Export…").size(13))
                .padding([5, 12])
                .style(button::secondary)
                .on_press_maybe(
                    (!settings.choosing_file && !self.feeds.is_empty()).then_some(Message::ExportOpml)
                ),
        ]
        .spacing(12)
        .align_y(Alignment::Center)]
        .spacing(6);
        if let Some((message, is_error)) = &settings.export_result {
            export_section = export_section.push(
                text(message)
                    .size(12)
                    .style(if *is_error { text::danger } else { text::success }),
            );
        }

        // Archive size, read fresh on each redraw: two file-size lookups, so
        // it's current after a refresh or a clear without any bookkeeping
        let archive_size = text(format!(
            "Archive size: {} · {}",
            format_size(self.db.disk_size()),
            plural(self.stats.1, "story", "stories"),
        ))
        .size(12);

        // Clear archived stories
        let clear_section: Element<'a, Message> = if let Some((to_delete, starred)) = settings.confirm_clear {
            let mut details = String::new();
            if settings.keep_starred && starred > 0 {
                details += &format!("Your {} will be kept. ", plural(starred, "starred story", "starred stories"));
            }
            details += "Feeds and folders stay. Stories your feeds still list will download again on the next refresh. This can't be undone.";
            column![
                text(format!("Delete {}?", plural(to_delete, "archived story", "archived stories"))).size(14),
                text(details).size(12).style(text::secondary),
                row![
                    horizontal_space(),
                    button(text("Cancel").size(13))
                        .padding([5, 12])
                        .style(button::text)
                        .on_press(Message::CancelClearStories),
                    button(text("Delete stories").size(13))
                        .padding([5, 12])
                        .style(button::danger)
                        .on_press_maybe((!self.is_syncing() && to_delete > 0).then_some(Message::ClearStories)),
                ]
                .spacing(8),
            ]
            .spacing(8)
            .into()
        } else {
            column![
                row![
                    column![
                        text("Clear archived stories").size(14),
                        text("Delete stored stories. Your feeds and folders are kept.")
                            .size(12)
                            .style(text::secondary),
                        archive_size,
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    button(text("Clear…").size(13))
                        .padding([5, 12])
                        .style(button::danger)
                        .on_press_maybe((!self.is_syncing()).then_some(Message::AskClearStories)),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
                checkbox("Keep starred stories", settings.keep_starred)
                    .on_toggle(Message::SetKeepStarred)
                    .size(14)
                    .text_size(12),
            ]
            .spacing(8)
            .into()
        };

        let note: Element<'a, Message> = if self.is_syncing() {
            text("Wait for the current refresh to finish before clearing stories.")
                .size(11)
                .style(text::secondary)
                .into()
        } else {
            Space::with_height(0).into()
        };

        // The sections scroll when the window is too short to show them all
        let sections = scrollable(
            column![
                refresh_section,
                horizontal_rule(1),
                images_section,
                horizontal_rule(1),
                notifications_section,
                horizontal_rule(1),
                tray_section,
                horizontal_rule(1),
                grouping_section,
                horizontal_rule(1),
                import_section,
                horizontal_rule(1),
                export_section,
                horizontal_rule(1),
                clear_section,
                note,
            ]
            .spacing(12),
        )
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::new().width(6).scroller_width(6).spacing(6),
        ));

        // Close sits beside the title: the sections come last, so they get
        // whatever height is left and scroll if it isn't enough
        let form = column![
            row![
                text("Settings").size(16),
                horizontal_space(),
                button(text("Close").size(13))
                    .padding([5, 14])
                    .style(button::secondary)
                    .on_press(Message::CloseSettings),
            ]
            .align_y(Alignment::Center),
            horizontal_rule(1),
            sections,
        ]
        .spacing(12);

        // Esc steps back out of the confirmation first, then closes
        modal(
            form,
            if settings.confirm_clear.is_some() {
                Message::CancelClearStories
            } else {
                Message::CloseSettings
            },
        )
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
                    (!self.is_syncing()).then_some(Message::RefreshFeed(feed.id)),
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
                (!self.is_syncing() && folder.feed_count > 0)
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

    pub fn view(&self, _window: iced::window::Id) -> Element<'_, Message> {
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

        // Left/Right arrows step through the story list, Up/Down scroll the
        // open story. Only keys nothing else used: a focused search or rename
        // field keeps its arrows.
        let content = on_key_press(content, move |key, modifiers| {
            use iced::keyboard::key::Named;
            match key.as_ref() {
                // Ctrl+F: find in the story
                iced::keyboard::Key::Character(c) if modifiers.command() && c.eq_ignore_ascii_case("f") => {
                    return Some(Message::OpenFind);
                }
                // Ctrl+Q: quit, even when closing the window wouldn't
                iced::keyboard::Key::Character(c) if modifiers.command() && c.eq_ignore_ascii_case("q") => {
                    return Some(Message::Quit);
                }
                _ => {}
            }
            if !modifiers.is_empty() {
                return None;
            }
            match key.as_ref() {
                iced::keyboard::Key::Named(Named::ArrowLeft) => Some(Message::SelectAdjacentStory(false)),
                iced::keyboard::Key::Named(Named::ArrowRight) => Some(Message::SelectAdjacentStory(true)),
                iced::keyboard::Key::Named(Named::ArrowUp) => Some(Message::ScrollReader(-READER_ARROW_STEP)),
                iced::keyboard::Key::Named(Named::ArrowDown) => Some(Message::ScrollReader(READER_ARROW_STEP)),
                _ => None,
            }
        })
        // The window's close button: the app decides between the tray and quitting
        .on_close_request(Message::WindowCloseRequested);

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
            // Dark like the story cards: red error text is hard to read on
            // the grey of `bordered_box`
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.base.color.into()),
                    border: iced::Border {
                        color: palette.background.strong.color,
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..container::Style::default()
                }
            });

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
        if let Some(settings) = &self.settings {
            layers.push(self.settings_view(settings));
        }

        let dragged = if let Some(feed_id) = self.dragging_feed {
            let title = self
                .feeds
                .iter()
                .find(|f| f.id == feed_id)
                .map(|f| f.title.as_str())
                .unwrap_or("Feed");
            Some((title, "Drop on a folder to move, or on a feed to reorder"))
        } else if let Some(folder_id) = self.dragging_folder {
            let name = self
                .folders
                .iter()
                .find(|f| f.id == folder_id)
                .map(|f| f.name.as_str())
                .unwrap_or("Folder");
            Some((name, "Drop on another folder to put it there"))
        } else {
            None
        };
        if let Some((feed_title, hint)) = dragged {

            let floating_badge = container(
                row![
                    grip_icon(true),
                    column![
                        text(format!("Dragging: {}", feed_title)).size(13).style(text::primary),
                        text(hint)
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
        let title_bar = row![
            column![
                text("Shreader RSS Reader").size(18),
                text("Permanent SQL Archive")
                    .size(11)
                    .style(text::secondary),
            ]
            .spacing(2),
            horizontal_space(),
            button(icon(Icon::Settings, 18.0, Tint::Text))
                .padding(6)
                .style(button::text)
                .on_press(Message::OpenSettings),
        ]
        .align_y(Alignment::Center);

        // Refresh Button
        let refresh_label: Element<'_, Message> = if self.is_syncing() {
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

        // From the archive, like the counts above (the story list only holds
        // the current view, and only its loaded part)
        let starred_btn = self.filter_button(
            ActiveFilter::Starred,
            Some(Icon::StarFilled),
            "Starred",
            &self.starred_total.to_string(),
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
        let is_dragging_folder = self.dragging_folder.is_some();

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

                let is_this_folder_dragged = self.dragging_folder == Some(folder.id);
                // Drag grip, like the feeds have, for reordering folders
                let grip = mouse_area(
                    container(grip_icon(is_this_folder_dragged))
                        .padding([4, 6])
                        .style(if is_this_folder_dragged {
                            container::bordered_box
                        } else {
                            container::transparent
                        }),
                )
                .on_press(Message::DragFolderStarted(folder.id))
                .interaction(if is_this_folder_dragged {
                    mouse::Interaction::Grabbing
                } else {
                    mouse::Interaction::Grab
                });

                let folder_header_row = row![
                    Element::from(grip),
                    Element::from(toggle_btn),
                    folder_btn,
                ]
                .spacing(2)
                .align_y(Alignment::Center);

                let folder_header = container(folder_header_row)
                    .width(Length::Fill)
                    .style(drop_zone_style(is_dragging, is_drop_hovered));

                // Insertion line while another folder is dragged over this one,
                // as an overlay so it never shifts the rows
                let insert_line: Element<'_, Message> = match self.folder_insert_edge(folder.id) {
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
                    .align_y(match edge {
                        InsertEdge::Top => iced::alignment::Vertical::Top,
                        InsertEdge::Bottom => iced::alignment::Vertical::Bottom,
                    })
                    .into(),
                    None => Space::new(Length::Shrink, Length::Shrink).into(),
                };

                let mut folder_drop_area = mouse_area(stack![folder_header, insert_line].width(Length::Fill));
                if is_dragging || (is_dragging_folder && !is_this_folder_dragged) {
                    folder_drop_area = folder_drop_area
                        .on_enter(Message::DragOverTarget(DropTarget::Folder(Some(folder.id))))
                        .on_exit(Message::DragLeaveTarget(DropTarget::Folder(Some(folder.id))))
                        .on_release(Message::DropOnTarget(DropTarget::Folder(Some(folder.id))))
                        .interaction(mouse::Interaction::Grabbing);
                }

                let menu = self
                    .folder_menu
                    .filter(|m| m.folder_id == folder.id && !is_dragging && !is_dragging_folder)
                    .map(|m| self.folder_menu_view(folder, m.confirm_delete));

                let mut header_with_menu = context_menu(folder_drop_area)
                    .menu(menu)
                    .on_dismiss(Message::CloseFolderMenu)
                    .on_escape(Message::EscapePressed);
                if !is_dragging && !is_dragging_folder {
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

        // Bottom: archive totals
        let db_status = column![
            horizontal_rule(1),
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
            ActiveFilter::All => format!("All Stories ({})", self.article_total),
            ActiveFilter::Unread => format!("Unread Stories ({})", self.article_total),
            ActiveFilter::Starred => format!("Starred Stories ({})", self.article_total),
            ActiveFilter::Folder(foid) => {
                let name = self
                    .folders
                    .iter()
                    .find(|f| f.id == foid)
                    .map(|f| f.name.as_str())
                    .unwrap_or("Folder");
                format!("{} ({})", name, self.article_total)
            }
            ActiveFilter::Feed(fid) => {
                let name = self
                    .feeds
                    .iter()
                    .find(|f| f.id == fid)
                    .map(|f| f.title.as_str())
                    .unwrap_or("Feed");
                format!("{} ({})", name, self.article_total)
            }
        };

        let mut header_label = row![].spacing(6).align_y(Alignment::Center).width(Length::Fill);
        if matches!(self.active_filter, ActiveFilter::Folder(_)) {
            header_label = header_label.push(icon(Icon::Folder, 14.0, Tint::Text));
        }
        header_label = header_label.push(text(list_header_label).size(14));

        let mut header_row = row![header_label].spacing(6).align_y(Alignment::Center);
        // Pointless in the Unread view, where everything is unread
        header_row = header_row.push(
            button(text("Grouped").size(12))
                .padding([4, 8])
                .style(if self.grouped { button::primary } else { button::secondary })
                .on_press(Message::ToggleGrouped),
        );
        if self.active_filter != ActiveFilter::Unread {
            header_row = header_row.push(
                button(text("Unread first").size(12))
                    .padding([4, 8])
                    // Highlighted while on
                    .style(if self.unread_first { button::primary } else { button::secondary })
                    .on_press(Message::ToggleUnreadFirst),
            );
        }
        let header_row = header_row.push(
            button(text("Mark All Read").size(12))
                .padding([4, 8])
                .style(button::secondary)
                .on_press(Message::MarkAllRead),
        );

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
            if self.grouped {
                for entry in &self.story_groups {
                    article_items = article_items.push(match entry.members.as_slice() {
                        [single] => self.story_list_card(single),
                        _ => self.group_list_card(entry),
                    });
                }
            } else {
                for article in self.articles.iter().take(self.article_render_limit) {
                    article_items = article_items.push(self.story_list_card(article));
                }
            }

            let remaining = self.article_total.saturating_sub(self.articles.len());
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

        // Room for the scrollbar, so it doesn't cover the cards' right outline
        let article_items = article_items.padding(iced::Padding { right: 12.0, ..iced::Padding::ZERO });
        let scroll_list = scrollable(article_items)
            .id(article_list_scroll_id())
            .width(Length::Fill)
            .height(Length::Fill);

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
        if let Some(group) = &self.reader_group {
            return self.view_reader_group(group);
        }
        let selected_article = self.open_article.clone();

        match selected_article {
            None => container(
                column![
                    text("No Story Selected").size(18),
                    text("Select an article from the list to view its contents.")
                        .size(13)
                        .style(text::secondary),
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
                    Some((id, blocks)) if *id == art.id => self.story_body(blocks, true, 0),
                    // Not prepared yet (bodies are loaded when a story is opened)
                    _ => Space::with_height(0).into(),
                };
                let date_str = art
                    .shown_date()
                    .map(format_date)
                    .unwrap_or_else(|| "Unknown date".to_string());
                let title = text(art.title).size(22);

                let feed = self.feeds.iter().find(|f| f.id == art.feed_id);
                let feed_title = art.feed_title.unwrap_or_else(|| "Feed".to_string());
                let folder_name = feed
                    .and_then(|f| f.folder_id)
                    .and_then(|foid| self.folders.iter().find(|fld| fld.id == foid))
                    .map(|fld| fld.name.clone());


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

                self.reader_frame(
                    column![title, meta_items.wrap(), action_row.wrap(), horizontal_rule(1)].spacing(12).into(),
                    body,
                    footer.into(),
                )
            }
        }
    }

    /// The reader around a story (or group): the find bar when open, the
    /// header, the scrolling body and the footer, plus the find keys.
    fn reader_frame<'a>(
        &'a self,
        header: Element<'a, Message>,
        body: Element<'a, Message>,
        footer: Element<'a, Message>,
    ) -> Element<'a, Message> {
        // Fill the reader's width: a shrink-sized scrollable is only as wide
        // as its text, which leaves the scrollbar mid-window for short stories
        // The scrollable spans the reader (scrollbar at the window edge); the
        // text inside it is a centered column
        let body_scroll = scrollable(reading_column(column![body].spacing(10).padding([10, 0])))
            .id(reader_scroll_id())
            .width(Length::Fill)
            .height(Length::Fill)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new().width(10).scroller_width(10).spacing(4),
            ));

        // The find bar goes in the header, not before it: widgets keep
        // their state (the story's scroll position) by their place in
        // the layout, so the scrollable's place mustn't change
        let mut top = column![].spacing(12);
        if let Some(find) = &self.find {
            top = top.push(self.find_bar(find));
        }
        let top = top.push(header);
        let reader = container(
            column![gutter(reading_column(top)), body_scroll, gutter(reading_column(footer))]
                .spacing(12)
                .padding(20),
        )
        .width(Length::Fill)
        .height(Length::Fill);
        // While the find bar is open: Esc closes it, and F3 / Shift+F3
        // or Shift+Enter go through matches, even while typing in it
        // (its text field would otherwise keep those keys). Always in
        // the layout, doing nothing while the bar is closed, so opening
        // and closing it doesn't move the story's scroll position.
        let find_open = self.find.is_some();
        on_key_press(reader, move |key, modifiers| {
            use iced::keyboard::key::Named;
            if !find_open {
                return None;
            }
            match key.as_ref() {
                iced::keyboard::Key::Named(Named::Escape) => Some(Message::CloseFind),
                iced::keyboard::Key::Named(Named::F3) if modifiers.shift() => Some(Message::FindPrevious),
                iced::keyboard::Key::Named(Named::F3) => Some(Message::FindNext),
                iced::keyboard::Key::Named(Named::Enter) if modifiers.shift() => Some(Message::FindPrevious),
                _ => None,
            }
        })
        .first()
        .into()
    }

    /// An open group in the reader: its stories one after another, oldest
    /// first, each under its source's name.
    fn view_reader_group<'a>(&'a self, group: &'a ReaderGroup) -> Element<'a, Message> {
        let mut sources: Vec<&str> = Vec::new();
        for (story, _) in &group.stories {
            let name = story.feed_title.as_deref().unwrap_or("Feed");
            if !sources.contains(&name) {
                sources.push(name);
            }
        }
        let header = column![
            text(&group.title).size(22),
            text(format!("{} stories from: {}", group.stories.len(), sources.join(", ")))
                .size(12)
                .style(text::secondary),
            horizontal_rule(1),
        ]
        .spacing(12);

        let mut body = column![].spacing(18).width(Length::Fill);
        let mut first_text = 0;
        for (index, (story, blocks)) in group.stories.iter().enumerate() {
            if index > 0 {
                body = body.push(horizontal_rule(1));
            }
            let mut source = row![
                text(story.feed_title.as_deref().unwrap_or("Feed")).size(13),
                text("•").size(12).style(text::secondary),
                text(story.shown_date().map(format_date).unwrap_or_else(|| "Unknown date".to_string()))
                    .size(12)
                    .style(text::secondary),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            if let Some(author) = &story.author {
                source = source.push(text("•").size(12).style(text::secondary));
                source = source.push(text(format!("By: {}", author)).size(12).style(text::secondary));
            }
            let mut actions = row![
                button(
                    row![
                        if story.is_starred {
                            icon(Icon::StarFilled, 12.0, Tint::Gold)
                        } else {
                            icon(Icon::Star, 12.0, Tint::OnSecondary)
                        },
                        text(if story.is_starred { "Starred" } else { "Star" }).size(12),
                    ]
                    .spacing(5)
                    .align_y(Alignment::Center),
                )
                .padding([4, 10])
                .style(button::secondary)
                .on_press(Message::ToggleStar(story.id)),
            ]
            .spacing(8);
            if let Some(url) = &story.url {
                actions = actions.push(
                    button(
                        row![icon(Icon::ExternalLink, 12.0, Tint::OnPrimary), text("Open in Browser").size(12)]
                            .spacing(5)
                            .align_y(Alignment::Center),
                    )
                    .padding([4, 10])
                    .style(button::primary)
                    .on_press(Message::OpenUrl(url.clone())),
                );
            }
            body = body.push(
                column![source.wrap(), text(&story.title).size(17), actions.wrap(), self.story_body(blocks, true, first_text)]
                    .spacing(10),
            );
            first_text += blocks.iter().filter(|b| matches!(b, Block::Text { .. })).count();
        }

        let footer = column![
            horizontal_rule(1),
            text("Grouped by the words the stories share").size(11).style(text::secondary),
        ]
        .spacing(4);
        self.reader_frame(header.into(), body.into(), footer.into())
    }

    pub fn theme(&self, _window: iced::window::Id) -> Theme {
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
    // Runs on the UI thread: a panic in the HTML conversion (a library bug
    // hit by odd story content) would close the app. Fall back to the plain
    // text stored for the story instead.
    let blocks = std::panic::catch_unwind(|| {
        article
            .content_html
            .as_deref()
            .or(article.summary.as_deref())
            .map(|html| story_content::blocks(html, article.url.as_deref()))
            .unwrap_or_default()
    })
    .unwrap_or_default();
    if !blocks.is_empty() {
        return blocks;
    }
    vec![Block::Text {
        text: article
            .content_text
            .clone()
            .unwrap_or_else(|| "No content available for this story.".to_string()),
        links: Vec::new(),
    }]
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

/// " Held back some new stories from …" for feeds that sent more than they
/// may add at once (empty if none did).
fn held_back_note(summaries: &[crate::models::FeedSyncSummary]) -> String {
    let mut limited: Vec<&str> = summaries.iter().filter(|s| s.limited).map(|s| s.feed_title.as_str()).collect();
    if limited.is_empty() {
        return String::new();
    }
    limited.sort_unstable_by_key(|t| t.to_lowercase());
    const NAMED: usize = 5;
    let mut names = limited.iter().take(NAMED).copied().collect::<Vec<_>>().join(", ");
    if limited.len() > NAMED {
        names += &format!(" and {} more", limited.len() - NAMED);
    }
    format!(
        " Held back some new stories from {} (more than a feed may add at once); later refreshes pick them up if the feed still lists them.",
        names
    )
}

/// Opens a web link in the default browser.
///
/// Links come from feeds, so only http(s) is passed on: `xdg-open` would also
/// open local files, launchers and other apps' URL schemes.
fn open_in_browser(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url.trim()).map_err(|_| format!("Not a valid link: {}", url))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("Only web links are opened, not '{}:' links", parsed.scheme()));
    }
    let mut child = std::process::Command::new("xdg-open")
        .arg(parsed.as_str())
        .spawn()
        .map_err(|e| format!("Couldn't start the browser: {}", e))?;
    // Collect its exit status, or it lingers as a zombie process until the app closes
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Runs a scroll task, then has the window redrawn.
///
/// iced applies scrolling (a widget operation) without asking for a redraw,
/// so on Wayland, where windows only redraw when asked, the new position
/// only appeared with the next event, such as moving the mouse. Any message
/// makes iced redraw.
fn then_redraw(scroll: Task<Message>) -> Task<Message> {
    scroll.chain(Task::done(Message::Noop))
}

/// Schedules `release_memory` shortly after a story with images is closed:
/// iced's renderer drops the images when it next draws without them, so
/// trimming right away would find nothing to give back.
fn release_memory_soon() -> Task<Message> {
    Task::perform(tokio::time::sleep(std::time::Duration::from_secs(2)), |_| {
        Message::ReleaseMemory
    })
}

/// Hands memory the allocator is holding on to back to the OS.
///
/// glibc keeps freed memory for reuse, so after viewing an image-heavy story
/// the process stayed hundreds of MB larger even though the images were gone.
fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free heap pages; no pointers are affected
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Story text size, and how it wraps (shared with `story_text`, whose
/// layout has to match the text editor's exactly)
const READER_TEXT_SIZE: f32 = 14.0;
const READER_TEXT_WRAPPING: iced::widget::text::Wrapping = iced::widget::text::Wrapping::WordOrGlyph;

/// Color of links in story text
const LINK_COLOR: Color = Color::from_rgb(0.45, 0.70, 1.0);

/// Colors the links in a story text block (the text editor's highlighter
/// hook): given each block's links, it marks their ranges line by line.
struct LinkHighlighter {
    links: std::sync::Arc<Vec<LinkSpan>>,
    line: usize,
}

impl iced::advanced::text::Highlighter for LinkHighlighter {
    type Settings = std::sync::Arc<Vec<LinkSpan>>;
    type Highlight = ();
    type Iterator<'a> = std::vec::IntoIter<(std::ops::Range<usize>, ())>;

    fn new(links: &Self::Settings) -> Self {
        Self { links: links.clone(), line: 0 }
    }

    fn update(&mut self, links: &Self::Settings) {
        self.links = links.clone();
        self.line = 0;
    }

    fn change_line(&mut self, line: usize) {
        self.line = line;
    }

    fn highlight_line(&mut self, _text: &str) -> Self::Iterator<'_> {
        let line = self.line;
        self.line += 1;
        self.links
            .iter()
            .filter(|link| link.line == line)
            .map(|link| (link.start..link.end, ()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn current_line(&self) -> usize {
        self.line
    }
}

/// Title color of a story that hasn't been read yet.
const UNREAD_TITLE: Color = Color::from_rgb(0.36, 0.84, 0.49);
/// Title color of a read story.
const READ_TITLE: Color = Color::WHITE;

/// A story in the list: the page background with a grey outline, so the
/// colored titles stay readable. Hovering brightens the outline; the open
/// story gets a thicker accent outline instead of a filled background.
fn story_card(theme: &Theme, status: button::Status, selected: bool) -> button::Style {
    let palette = theme.extended_palette();
    let (color, width) = if selected {
        (palette.primary.base.color, 2.0)
    } else {
        match status {
            button::Status::Hovered | button::Status::Pressed => (palette.background.strong.text.scale_alpha(0.6), 1.0),
            _ => (palette.background.strong.color, 1.0),
        }
    };
    button::Style {
        background: Some(palette.background.base.color.into()),
        text_color: palette.background.base.text,
        border: iced::Border {
            color,
            width,
            radius: 4.0.into(),
        },
        ..button::Style::default()
    }
}

/// The story list's scrollable.
fn article_list_scroll_id() -> scrollable::Id {
    scrollable::Id::new("article-list")
}

/// The open story's reader.
fn reader_scroll_id() -> scrollable::Id {
    scrollable::Id::new("reader")
}

/// How far one Up/Down arrow press scrolls the open story, in pixels
/// (about three lines; holding the key repeats it).
const READER_ARROW_STEP: f32 = 60.0;

/// A story's card in the list.
fn story_card_id(article_id: i64) -> container::Id {
    container::Id::new(format!("story-{}", article_id))
}

/// A group's card in the grouped list.
fn group_card_id(group: i64) -> container::Id {
    container::Id::new(format!("group-{}", group))
}

/// The current find match (reported by `story_text`, to scroll to it)
fn find_match_id() -> container::Id {
    container::Id::new("find-match")
}

#[cfg(test)]
mod tests {
    use super::{format_size, open_in_browser, ImageState, RefreshInterval, RssApp};

    /// The sidebar's Starred count covers the whole archive, whichever view
    /// is open.
    #[test]
    fn starred_count_is_for_the_whole_archive() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let folder_a = db.create_folder("A").unwrap();
        let folder_b = db.create_folder("B").unwrap();
        for (url, folder) in [("https://a.example/rss", folder_a), ("https://b.example/rss", folder_b)] {
            let feed = db.add_or_update_feed(url, url, None, None, Some(folder)).unwrap();
            for n in 0..3 {
                let hash = format!("{}-{}", url, n);
                db.upsert_article(feed, None, &hash, &hash, None, None, None, None, None, None, None).unwrap();
            }
        }
        let (mut app, _) = RssApp::new(db.clone(), false);
        // Star one story in each folder, from folder A's view
        let in_a = app.articles[0].id;
        let _ = app.update(super::Message::ToggleStar(in_a));
        let in_b = db
            .get_articles(&crate::models::ArticleFilter { folder_id: Some(folder_b), ..Default::default() })
            .unwrap()[0]
            .id;
        let _ = app.update(super::Message::ToggleStar(in_b));
        for filter in [
            super::ActiveFilter::Folder(folder_a),
            super::ActiveFilter::Folder(folder_b),
            super::ActiveFilter::Starred,
            super::ActiveFilter::Unread,
        ] {
            let _ = app.update(super::Message::SelectFilter(filter.clone()));
            assert_eq!(app.starred_total, 2, "in {:?}", filter);
        }
    }

    /// The grouped view: a group's stories form one entry, opening it reads
    /// them all and shows them oldest first, and the arrows step by entry.
    #[test]
    fn grouped_view_works() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, None).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, None).unwrap();
        let base = chrono::Utc::now() - chrono::Duration::hours(10);
        let add = |feed: i64, hash: &str, hours: i64, title: &str| {
            db.upsert_article(feed, None, hash, title, None, None, Some(title), None, Some(title),
                Some(base + chrono::Duration::hours(hours)), None).unwrap();
        };
        add(reuters, "a", 0, "Spirit Airlines data sold to Google in bankruptcy sale");
        add(verge, "b", 1, "Google buys Spirit Airlines data in bankruptcy deal");
        add(verge, "c", 2, "Apple releases a new iPad with a faster chip");
        crate::story_groups::group_new_stories(&db).unwrap();

        let (mut app, _) = RssApp::new(db, false);
        let _ = app.update(super::Message::SelectFilter(super::ActiveFilter::All));
        let _ = app.update(super::Message::ToggleGrouped);
        let sizes: Vec<usize> = app.story_groups.iter().map(|g| g.members.len()).collect();
        assert_eq!(sizes, [1, 2], "the iPad story, then the Spirit group");
        assert_eq!(app.story_groups[1].title(), "Spirit Airlines data sold to Google in bankruptcy sale");

        let group = app.story_groups[1].group;
        let _ = app.update(super::Message::SelectGroup(group));
        let open = app.reader_group.as_ref().expect("group open");
        let titles: Vec<&str> = open.stories.iter().map(|(a, _)| a.title.as_str()).collect();
        assert_eq!(titles[0], "Spirit Airlines data sold to Google in bankruptcy sale", "oldest first");
        assert_eq!(app.story_groups[1].unread(), 0, "opening the group read it");
        assert_eq!(app.stats.2, 1, "one unread story left");

        // Left goes back to the iPad story
        let _ = app.update(super::Message::SelectAdjacentStory(false));
        assert!(app.reader_group.is_none());
        assert_eq!(app.open_article.as_ref().map(|a| a.title.as_str()), Some("Apple releases a new iPad with a faster chip"));
        assert_eq!(app.story_groups[0].unread(), 0, "read in the grouped list too");
    }

    /// Opening a different story closes the find bar; reopening the same
    /// one keeps it.
    #[test]
    fn find_bar_closes_on_another_story() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://f.example/rss", "F", None, None, None).unwrap();
        for n in 0..2 {
            let hash = format!("story-{}", n);
            db.upsert_article(feed, None, &hash, &hash, None, None, Some("<p>the cat sat</p>"), None, None, None, None)
                .unwrap();
        }
        let (mut app, _) = RssApp::new(db, false);
        let _ = app.update(super::Message::SelectFilter(super::ActiveFilter::All));
        let (first, second) = (app.articles[0].id, app.articles[1].id);
        let _ = app.update(super::Message::SelectArticle(first));
        let _ = app.update(super::Message::OpenFind);
        let _ = app.update(super::Message::FindQueryChanged("cat".into()));
        assert_eq!(app.find.as_ref().map(|f| f.total()), Some(1));
        let _ = app.update(super::Message::SelectArticle(first));
        assert_eq!(app.find.as_ref().map(|f| f.total()), Some(1), "same story: kept");
        let _ = app.update(super::Message::SelectArticle(second));
        assert!(app.find.is_none(), "another story: closed");
    }

    /// With notifications off, no message pops up; turning them back on
    /// shows the next one.
    #[tokio::test]
    async fn notifications_can_be_silenced() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let (mut app, _) = RssApp::new(db, false);
        let _ = app.update(super::Message::SetNotifications(false));
        let _ = app.update(super::Message::OpenUrl("file:///etc/passwd".into())); // refused: an error message
        assert_eq!(app.status_message, None);
        let _ = app.update(super::Message::SetNotifications(true));
        let _ = app.update(super::Message::OpenUrl("file:///etc/passwd".into()));
        assert!(app.status_message.is_some());
    }

    /// Story images download a few at a time, in order; switching stories
    /// cancels downloads and queued images the new story doesn't show.
    #[test]
    fn story_images_download_a_few_at_a_time() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let (mut app, _) = RssApp::new(db, false);
        for n in 0..10 {
            let _ = app.update(super::Message::LoadStoryImage(format!("https://example.com/{}.png", n)));
        }
        assert_eq!(app.image_downloads.len(), super::MAX_IMAGE_DOWNLOADS);
        assert_eq!(app.image_queue.front().map(String::as_str), Some("https://example.com/4.png"));
        assert_eq!(app.image_queue.len(), 10 - super::MAX_IMAGE_DOWNLOADS);

        // A story showing images 1 and 7 only
        let keep = ["https://example.com/1.png", "https://example.com/7.png"];
        let blocks: Vec<_> = keep.iter().map(|src| crate::story_content::Block::Image { src: src.to_string() }).collect();
        app.prune_story_images(&blocks);
        assert_eq!(app.image_downloads.keys().map(String::as_str).collect::<Vec<_>>(), [keep[0]]);
        assert_eq!(app.image_queue, [keep[1]]);
    }

    /// Past the memory budget, the images loaded longest ago are released
    /// (keeping their size for the layout); the newest is never released.
    #[test]
    fn story_images_stay_within_memory_budget() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let (mut app, _) = RssApp::new(db, false);
        // 20 images of 1600x4800 (~30 MB each, ~614 MB in all). The handles
        // are 1x1: the budget goes by the recorded size.
        for n in 1..=20u64 {
            app.story_images.insert(
                format!("img{}", n),
                ImageState::Raster {
                    handle: iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]),
                    width: 1600,
                    height: 4800,
                    order: n,
                },
            );
        }
        assert!(app.release_story_images_over_budget("img20"));
        let released: Vec<u64> = (1..=20)
            .filter(|n| matches!(app.story_images[&format!("img{}", n)], ImageState::Released { width: 1600, height: 4800 }))
            .collect();
        assert_eq!(released, (1..=7).collect::<Vec<_>>());
        // Within budget now: nothing more to release
        assert!(!app.release_story_images_over_budget("img20"));
    }

    #[test]
    fn only_web_links_are_opened() {
        // Refused before anything is launched
        for url in ["file:///etc/passwd", "javascript:alert(1)", "-x", "/home/user/run.desktop", "steam://run/1"] {
            assert!(open_in_browser(url).is_err(), "{}", url);
        }
    }

    #[test]
    fn size_labels() {
        assert_eq!(format_size(0), "1 KB");
        assert_eq!(format_size(86 * 1024), "86 KB");
        assert_eq!(format_size(4_404_019), "4.2 MB");
        assert_eq!(format_size(60_659_000), "58 MB");
        assert_eq!(format_size(1_395_864_371), "1.3 GB");
    }

    #[test]
    fn refresh_interval_labels() {
        let label = |m| RefreshInterval(m).to_string();
        assert_eq!(label(1), "1 minute");
        assert_eq!(label(30), "30 minutes");
        assert_eq!(label(60), "1 hour");
        assert_eq!(label(90), "90 minutes");
        assert_eq!(label(720), "12 hours");
        assert_eq!(label(1440), "1 day");
        assert_eq!(label(2880), "2 days");
    }
}

/// A byte count for people: "850 KB", "4.2 MB", "58 MB", "1.3 GB".
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= 10.0 * MB {
        format!("{:.0} MB", b / MB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", (b / KB).max(1.0))
    }
}
