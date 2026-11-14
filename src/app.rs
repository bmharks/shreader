use std::collections::HashSet;
use std::sync::Arc;
use chrono::{DateTime, Utc};
use iced::mouse;
use iced::widget::{
    button, column, container, horizontal_rule, horizontal_space, mouse_area, row, scrollable,
    stack, text, text_input, vertical_rule, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Vector};

use crate::db::Database;
use crate::feed::FeedFetcher;
use crate::follow_cursor::follow_cursor;
use crate::models::{Article, ArticleFilter, Feed, Folder, FeedSyncSummary};

const FOLDER_INPUT_ID: &str = "new_folder_input_id";

/// How many article cards to render at a time. Every message rebuilds and
/// re-lays out the whole widget tree, so rendering thousands of cards made each
/// update cost 100+ ms (and made drag-and-drop lag). More load on demand.
const ARTICLE_PAGE_SIZE: usize = 150;

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
    search_query: String,
    new_feed_input: String,
    new_folder_input: String,
    show_new_folder_input: bool,
    moving_feed_id: Option<i64>,
    is_syncing: bool,
    status_message: Option<(String, bool)>,
    stats: (i64, i64, i64), // (total_feeds, total_articles, unread_articles)

    // Drag-and-drop state
    dragging_feed: Option<i64>,                 // feed_id being dragged
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
    NewFeedInputChanged(String),
    AddFeedClicked,
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
    ShowMoreArticles,

    // Folder Actions
    ToggleFolderExpand(i64),
    ShowNewFolderInput,
    CancelNewFolderInput,
    NewFolderInputChanged(String),
    CreateFolderClicked,
    DeleteFolder(i64),
    StartMovingFeed(i64),
    AssignFeedToFolder(i64, Option<i64>),
    CancelMoveFeed,

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
            search_query: String::new(),
            new_feed_input: String::new(),
            new_folder_input: String::new(),
            show_new_folder_input: false,
            moving_feed_id: None,
            is_syncing: false,
            status_message: None,
            stats: (0, 0, 0),
            dragging_feed: None,
            drag_hover_target: None,
        };

        app.reload_data();

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
        // Drag release is reported by the floating badge widget (follow_cursor.rs)
        // rather than a global event listener, which would also be fed every
        // mouse move and can overflow its channel ("TrySendError { kind: Full }").
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
        match message {
            Message::SelectFilter(filter) => {
                self.active_filter = filter;
                self.article_render_limit = ARTICLE_PAGE_SIZE;
                self.reload_articles();
                Task::none()
            }
            Message::SelectArticle(article_id) => {
                self.selected_article_id = Some(article_id);
                // Mark as read when selected
                let _ = self.db.set_article_read(article_id, true);
                self.reload_data();
                Task::none()
            }
            Message::NewFeedInputChanged(url) => {
                self.new_feed_input = url;
                Task::none()
            }
            Message::AddFeedClicked => {
                let url = self.new_feed_input.trim().to_string();
                if url.is_empty() {
                    return Task::none();
                }

                // If a folder is currently selected, add the new feed into that folder!
                let folder_id = match self.active_filter {
                    ActiveFilter::Folder(fid) => Some(fid),
                    ActiveFilter::Feed(fid) => {
                        self.feeds.iter().find(|f| f.id == fid).and_then(|f| f.folder_id)
                    }
                    _ => None,
                };

                self.new_feed_input.clear();
                self.is_syncing = true;
                let db_clone = self.db.clone();
                let fetcher_clone = self.fetcher.clone();

                Task::perform(
                    async move {
                        // 1. Add feed placeholder
                        let feed_id = db_clone
                            .add_or_update_feed(&url, &url, None, None, folder_id)
                            .map_err(|e| format!("Database error: {}", e))?;

                        // 2. Fetch and sync feed items
                        fetcher_clone
                            .fetch_and_sync(&db_clone, feed_id)
                            .await
                            .map_err(|e| format!("Feed fetch error: {}", e))?;

                        Ok((feed_id, url))
                    },
                    Message::FeedAdded,
                )
            }
            Message::FeedAdded(result) => {
                self.is_syncing = false;
                match result {
                    Ok((_feed_id, url)) => {
                        self.status_message = Some((format!("Subscribed to: {}", url), false));
                        self.reload_data();
                    }
                    Err(e) => {
                        self.status_message = Some((e, true));
                    }
                }
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
                let _ = self.db.delete_feed(feed_id);
                if self.active_filter == ActiveFilter::Feed(feed_id) {
                    self.active_filter = ActiveFilter::All;
                }
                self.reload_data();
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
                let _ = self.db.delete_folder(folder_id);
                if self.active_filter == ActiveFilter::Folder(folder_id) {
                    self.active_filter = ActiveFilter::All;
                }
                self.reload_data();
                Task::none()
            }
            Message::StartMovingFeed(feed_id) => {
                self.moving_feed_id = Some(feed_id);
                Task::none()
            }
            Message::AssignFeedToFolder(feed_id, folder_id) => {
                let _ = self.db.set_feed_folder(feed_id, folder_id);
                self.moving_feed_id = None;
                self.reload_data();
                Task::none()
            }
            Message::CancelMoveFeed => {
                self.moving_feed_id = None;
                Task::none()
            }

            // Drag-and-Drop Handlers
            Message::DragFeedStarted(feed_id) => {
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

        let feed_btn = button(
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
        .on_press(Message::SelectFilter(ActiveFilter::Feed(feed.id)));

        let move_btn = button(text("⇄").size(11))
            .padding([2, 4])
            .style(button::text)
            .on_press(Message::StartMovingFeed(feed.id));

        let del_btn = button(text("×").size(12))
            .padding([2, 4])
            .style(button::text)
            .on_press(Message::DeleteFeed(feed.id));

        let feed_row = row![
            Space::with_width(if indented { 12 } else { 0 }),
            Element::from(grip_btn),
            Element::from(feed_btn),
            Element::from(move_btn),
            Element::from(del_btn),
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
        drop_area.into()
    }

    pub fn view(&self) -> Element<'_, Message> {
        let sidebar = self.view_sidebar();
        let article_list = self.view_article_list();
        let reader_view = self.view_reader();

        let content = row![
            sidebar,
            vertical_rule(1),
            article_list,
            vertical_rule(1),
            reader_view,
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

        // Add Feed Input
        let add_feed_row = row![
            text_input("Add feed URL...", &self.new_feed_input)
                .on_input(Message::NewFeedInputChanged)
                .on_submit(Message::AddFeedClicked)
                .padding(6)
                .size(13),
            button(text("+ Add").size(13))
                .padding([6, 12])
                .on_press(Message::AddFeedClicked),
        ]
        .spacing(6);

        // Refresh Button
        let refresh_label = if self.is_syncing {
            "Syncing Feeds..."
        } else {
            "⟳ Refresh All Feeds"
        };
        let refresh_btn = button(text(refresh_label).size(13))
            .width(Length::Fill)
            .padding([6, 10])
            .on_press(Message::RefreshClicked);

        // Core Category Filters
        let (total_feeds, total_articles, unread_articles) = self.stats;

        let all_btn = self.filter_button(
            ActiveFilter::All,
            "All Stories",
            &total_articles.to_string(),
        );

        let unread_btn = self.filter_button(
            ActiveFilter::Unread,
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
            "★ Starred",
            &starred_count.to_string(),
        );

        let categories = column![all_btn, unread_btn, starred_btn].spacing(4);

        // Subscriptions & Folders Header
        let subscriptions_header = row![
            text("SUBSCRIPTIONS").size(11).style(text::secondary),
            horizontal_space(),
            button(text("+ Folder").size(11))
                .padding([2, 6])
                .style(button::secondary)
                .on_press(Message::ShowNewFolderInput),
        ]
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

        // Folder Moving Overlay Prompt (Accessible click alternative)
        if let Some(target_feed_id) = self.moving_feed_id {
            let feed_title = self
                .feeds
                .iter()
                .find(|f| f.id == target_feed_id)
                .map(|f| f.title.as_str())
                .unwrap_or("Feed");

            let mut move_dialog = column![
                text(format!("Move '{}' to:", feed_title)).size(12).style(text::primary),
                button(text("(No Folder / Uncategorized)").size(11))
                    .width(Length::Fill)
                    .padding([3, 6])
                    .style(button::secondary)
                    .on_press(Message::AssignFeedToFolder(target_feed_id, None)),
            ]
            .spacing(3);

            for fld in &self.folders {
                move_dialog = move_dialog.push(
                    button(text(format!("📁 {}", fld.name)).size(11))
                        .width(Length::Fill)
                        .padding([3, 6])
                        .style(button::secondary)
                        .on_press(Message::AssignFeedToFolder(target_feed_id, Some(fld.id))),
                );
            }

            move_dialog = move_dialog.push(
                button(text("Cancel").size(10))
                    .padding([2, 6])
                    .style(button::text)
                    .on_press(Message::CancelMoveFeed),
            );

            tree_items = tree_items.push(
                container(move_dialog)
                    .padding(6)
                    .style(container::bordered_box),
            );
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

                let folder_btn = button(
                    row![
                        text(format!("📁 {}", folder.name)).size(13).width(Length::Fill),
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
                .on_press(Message::SelectFilter(ActiveFilter::Folder(folder.id)));

                let delete_fld_btn = button(text("×").size(12))
                    .padding([2, 5])
                    .style(button::text)
                    .on_press(Message::DeleteFolder(folder.id));

                let folder_header_row = row![
                    Element::from(toggle_btn),
                    Element::from(folder_btn),
                    Element::from(delete_fld_btn),
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

                tree_items = tree_items.push(folder_drop_area);
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
                add_feed_row,
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
        label: &str,
        badge: &str,
    ) -> Element<'_, Message> {
        let is_selected = self.active_filter == target_filter;
        let label_owned = label.to_string();
        let badge_owned = badge.to_string();

        button(
            row![
                text(label_owned).size(13).width(Length::Fill),
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
                format!("📁 {} ({})", name, self.articles.len())
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

        let header_row = row![
            text(list_header_label).size(14).width(Length::Fill),
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
                "No feeds added yet. Paste a feed URL on the left to start archiving!"
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
                let star_symbol = if article.is_starred { "★" } else { "☆" };

                let feed_name = article.feed_title.as_deref().unwrap_or("RSS");
                let date_str = article
                    .published_at
                    .map(format_date)
                    .unwrap_or_else(|| "Unknown date".to_string());

                let star_btn = button(text(star_symbol).size(14))
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

                article_items = article_items.push(item_btn);
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

        let scroll_list = scrollable(article_items).height(Length::Fill);

        container(
            column![search_box, header_row, horizontal_rule(1), scroll_list]
                .spacing(8)
                .padding(10),
        )
        .width(Length::Fixed(360.0))
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
                    text("🔒 Archival Guarantee: Every story fetched is stored in SQLite and will remain accessible even if the source feed removes it.")
                        .size(12)
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
                    meta_items = meta_items.push(text(format!("📁 {}", fld)).size(12).style(text::secondary));
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
                    button(text(if art.is_starred { "★ Starred" } else { "☆ Star" }).size(12))
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::ToggleStar(art.id)),
                    button(text("⇄ Move Feed").size(12))
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::StartMovingFeed(art.feed_id)),
                ]
                .spacing(8);

                if let Some(url) = art.url {
                    let open_btn = button(text("↗ Open in Browser").size(12))
                        .padding([4, 10])
                        .style(button::primary)
                        .on_press(Message::OpenUrl(url));
                    action_row = action_row.push(open_btn);
                }

                // Body text (clean text extracted from HTML)
                let body_str = art
                    .content_text
                    .or(art.summary)
                    .unwrap_or_else(|| "No content available for this story.".to_string());

                let body = text(body_str).size(14);

                let body_scroll = scrollable(
                    column![body].spacing(10).padding([10, 15]),
                )
                .height(Length::Fill);

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
                        title,
                        meta_items,
                        action_row,
                        horizontal_rule(1),
                        body_scroll,
                        footer,
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
