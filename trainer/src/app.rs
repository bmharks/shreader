//! The trainer's window: pick a tag and the folders it sorts, answer
//! whether stories are in it, train its model, and put the model to use.

use std::collections::HashSet;

use iced::widget::{
    Space, button, checkbox, column, container, horizontal_rule, pick_list, row, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme, keyboard};
use shreader_tags::model::{MIN_EACH, TagModel, Trained};
use shreader_tags::store::{self, Tag};

use crate::archive::{Answered, Archive, Counts, Folder, Story};
use crate::queue::{Order, arrange};

/// After this many new answers, the draft model is retrained by itself (so
/// "least sure" keeps up with what you've taught it)
const AUTO_TRAIN_EVERY: usize = 10;

/// A model is only offered for use if, on answers it didn't learn from, at
/// least this share of the stories it tagged should have been (so it
/// doesn't hide real stories in Shreader)
const MIN_PRECISION: f32 = 0.8;

/// Answers listed under "Latest answers"
const RECENT: usize = 12;

pub struct Trainer {
    archive: Archive,
    /// An error or notice, shown at the top
    notice: Option<String>,
    /// Stories being prepared (embedded) in the background
    preparing: bool,
    tags: Vec<Tag>,
    tag: Option<i64>,
    /// The name being typed for a new tag
    new_tag: Option<String>,
    /// Asking whether to delete the tag
    confirm_delete: bool,
    folders: Vec<Folder>,
    folder_tags: Vec<(i64, i64)>,
    /// The tag's stories still to review (newest first), with embeddings
    candidates: Vec<(i64, Vec<f32>)>,
    /// Embeddings of stories said to be in the tag
    positives: Vec<Vec<f32>>,
    counts: Counts,
    recent: Vec<(i64, String, bool)>,
    /// The model in use in Shreader: (model, summary, when trained)
    published: Option<(TagModel, String, String)>,
    /// The latest training, and how many answers it learned from
    draft: Option<(Trained, usize)>,
    /// Whether that's the model in use (it's been put to use since)
    draft_in_use: bool,
    training: bool,
    order: Order,
    /// What's next, after `current`
    queue: Vec<i64>,
    skipped: HashSet<i64>,
    search: String,
    /// The search being reviewed, if any (its results are the queue)
    searching: Option<String>,
    current: Option<Story>,
    /// The story being loaded to show
    pending: Option<i64>,
    /// Put this story first next time the queue is arranged (after an undo)
    front: Option<i64>,
    undo: Vec<Answered>,
}

/// A tag, as listed in the picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagChoice(Tag);

impl std::fmt::Display for TagChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.name)
    }
}

/// What the window shows about a tag, loaded together
#[derive(Debug, Clone)]
pub struct TagData {
    tag: i64,
    candidates: Vec<(i64, Vec<f32>)>,
    positives: Vec<Vec<f32>>,
    counts: Counts,
    recent: Vec<(i64, String, bool)>,
    published: Option<(TagModel, String, String)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Loaded(Result<(Vec<Tag>, Vec<Folder>, Vec<(i64, i64)>), String>),
    ModelReady(Result<(), String>),
    Prepared(Result<usize, String>),
    PickTag(TagChoice),
    StartNewTag,
    NewTagName(String),
    CreateTag,
    CancelNewTag,
    TagCreated(Result<i64, String>),
    AskDeleteTag,
    CancelDeleteTag,
    /// Deletes the tag; `true`: keeping the answers about it and its model
    DeleteTag(bool),
    /// Whether the answers and model were kept
    TagDeleted(Result<bool, String>),
    ToggleFolder(i64, bool),
    FolderToggled(Result<(), String>),
    TagLoaded(Result<TagData, String>),
    PickOrder(Order),
    SearchText(String),
    Search,
    Searched(Result<(String, Vec<i64>), String>),
    ClearSearch,
    StoryLoaded(i64, Result<Option<Story>, String>),
    Answer(bool),
    Answered(Result<Answered, String>),
    Skip,
    Undo,
    Undone(Result<i64, String>),
    Review(i64),
    Train,
    /// The model, and how many answers it learned from
    Trained(Result<(Trained, usize), String>),
    Publish,
    Published(Result<(), String>),
    DismissNotice,
}

fn err(e: anyhow::Error) -> String {
    format!("{:#}", e)
}

/// Runs `work` on a blocking thread (database and model work)
fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    done: impl Fn(Result<T, String>) -> Message + Send + 'static,
) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || work().map_err(err)).await.unwrap_or_else(|e| Err(e.to_string()))
        },
        done,
    )
}

impl Trainer {
    pub fn new(archive: Archive) -> (Self, Task<Message>) {
        let trainer = Self {
            archive,
            notice: None,
            preparing: true,
            tags: Vec::new(),
            tag: None,
            new_tag: None,
            confirm_delete: false,
            folders: Vec::new(),
            folder_tags: Vec::new(),
            candidates: Vec::new(),
            positives: Vec::new(),
            counts: Counts::default(),
            recent: Vec::new(),
            published: None,
            draft: None,
            draft_in_use: false,
            training: false,
            order: Order::LeastSure,
            queue: Vec::new(),
            skipped: HashSet::new(),
            search: String::new(),
            searching: None,
            current: None,
            pending: None,
            front: None,
            undo: Vec::new(),
        };
        let dir = trainer.archive.model_dir();
        let tasks = Task::batch([
            trainer.load(),
            Task::perform(
                async move { shreader_tags::embedder::ensure_model(&dir, download).await },
                Message::ModelReady,
            ),
        ]);
        (trainer, tasks)
    }

    fn load(&self) -> Task<Message> {
        let archive = self.archive.clone();
        blocking(
            move || {
                let (tags, folder_tags) = {
                    let conn = archive.lock();
                    (store::tags(&conn)?, store::folder_tags(&conn)?)
                };
                Ok((tags, archive.folders()?, folder_tags))
            },
            Message::Loaded,
        )
    }

    fn load_tag(&self) -> Task<Message> {
        let Some(tag) = self.tag else { return Task::none() };
        let archive = self.archive.clone();
        blocking(
            move || {
                let published = store::model(&archive.lock(), tag)?;
                Ok(TagData {
                    tag,
                    candidates: archive.candidates(tag)?,
                    positives: archive.positives(tag)?,
                    counts: archive.counts(tag)?,
                    recent: archive.recent_answers(tag, RECENT)?,
                    published,
                })
            },
            Message::TagLoaded,
        )
    }

    fn prepare(&mut self) -> Task<Message> {
        self.preparing = true;
        let archive = self.archive.clone();
        blocking(move || archive.embed_new_stories(), Message::Prepared)
    }

    fn tag_name(&self) -> &str {
        self.tags.iter().find(|t| Some(t.id) == self.tag).map(|t| t.name.as_str()).unwrap_or("the tag")
    }

    fn answers(&self) -> usize {
        self.counts.yes + self.counts.no
    }

    fn can_train(&self) -> bool {
        self.counts.yes >= MIN_EACH && self.counts.no >= MIN_EACH
    }

    /// The model that orders the queue and gives its guesses: the latest
    /// training, or else the one in use
    fn guide(&self) -> Option<&TagModel> {
        self.draft.as_ref().map(|(t, _)| &t.model).or(self.published.as_ref().map(|(m, _, _)| m))
    }

    /// The story showing, or being loaded to show
    fn showing(&self) -> Option<i64> {
        self.current.as_ref().map(|s| s.id).or(self.pending)
    }

    /// Puts the queue in order and, if no story is showing, shows the first.
    fn arrange_queue(&mut self) -> Task<Message> {
        let mut queue: Vec<i64> = match &self.searching {
            Some(_) => self.queue.clone(),
            None => arrange(&self.candidates, self.order, self.guide(), &self.positives),
        };
        queue.retain(|id| !self.skipped.contains(id));
        if let Some(front) = self.front.take() {
            // In place of the story showing, which goes back in the queue
            if let Some(showing) = self.showing().filter(|_| self.searching.is_some()) {
                queue.insert(0, showing);
            }
            queue.retain(|&id| id != front);
            queue.insert(0, front);
            self.queue = queue;
            return self.next_story();
        }
        let showing = self.showing();
        queue.retain(|&id| Some(id) != showing);
        self.queue = queue;
        if showing.is_none() { self.next_story() } else { Task::none() }
    }

    /// Shows the next story in the queue.
    fn next_story(&mut self) -> Task<Message> {
        self.current = None;
        self.pending = None;
        if self.queue.is_empty() {
            return Task::none();
        }
        let id = self.queue.remove(0);
        self.pending = Some(id);
        let archive = self.archive.clone();
        blocking(move || archive.story(id), move |r| Message::StoryLoaded(id, r))
    }

    fn train(&mut self) -> Task<Message> {
        let Some(tag) = self.tag else { return Task::none() };
        if self.training || !self.can_train() {
            return Task::none();
        }
        self.training = true;
        let answers = self.answers();
        let archive = self.archive.clone();
        blocking(
            move || {
                let examples = store::training_examples(&archive.lock(), tag)?;
                Ok((TagModel::train(&examples)?, answers))
            },
            Message::Trained,
        )
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(Ok((tags, folders, folder_tags))) => {
                self.tags = tags;
                self.folders = folders;
                self.folder_tags = folder_tags;
                if self.tag.is_none_or(|t| !self.tags.iter().any(|tag| tag.id == t)) {
                    self.tag = self.tags.first().map(|t| t.id);
                }
                self.load_tag()
            }
            Message::ModelReady(Ok(())) => self.prepare(),
            Message::ModelReady(Err(e)) => {
                self.preparing = false;
                self.notice = Some(format!("Couldn't get the language model the trainer needs: {}", e));
                Task::none()
            }
            Message::Prepared(result) => {
                self.preparing = false;
                if let Err(e) = result {
                    self.notice = Some(format!("Couldn't prepare stories for review: {}", e));
                }
                self.load_tag()
            }
            Message::PickTag(TagChoice(tag)) => {
                self.confirm_delete = false;
                if self.tag != Some(tag.id) {
                    self.tag = Some(tag.id);
                    self.draft = None;
                    self.undo.clear();
                    self.skipped.clear();
                    self.searching = None;
                    self.current = None;
                    self.pending = None;
                    self.queue.clear();
                }
                self.load_tag()
            }
            Message::StartNewTag => {
                self.new_tag = Some(String::new());
                text_input::focus("new-tag")
            }
            Message::NewTagName(name) => {
                self.new_tag = Some(name);
                Task::none()
            }
            Message::CancelNewTag => {
                self.new_tag = None;
                Task::none()
            }
            Message::CreateTag => {
                let Some(name) = self.new_tag.clone().filter(|n| !n.trim().is_empty()) else { return Task::none() };
                let archive = self.archive.clone();
                blocking(move || store::create_tag(&archive.lock(), &name), Message::TagCreated)
            }
            Message::TagCreated(Ok(id)) => {
                self.new_tag = None;
                self.tag = Some(id);
                self.draft = None;
                self.undo.clear();
                self.current = None;
                self.pending = None;
                self.queue.clear();
                self.load()
            }
            Message::AskDeleteTag => {
                self.confirm_delete = true;
                Task::none()
            }
            Message::CancelDeleteTag => {
                self.confirm_delete = false;
                Task::none()
            }
            Message::DeleteTag(keep_training) => {
                self.confirm_delete = false;
                let Some(tag) = self.tag else { return Task::none() };
                let archive = self.archive.clone();
                blocking(
                    move || {
                        let conn = archive.lock();
                        if keep_training {
                            store::delete_tag(&conn, tag)?;
                        } else {
                            store::delete_tag_and_training(&conn, tag)?;
                        }
                        Ok(keep_training)
                    },
                    Message::TagDeleted,
                )
            }
            Message::TagDeleted(Ok(kept)) => {
                self.notice = Some(if kept {
                    format!(
                        "Deleted {}. Its stories are back in their folders. Your answers and its model are kept: make a tag called {} again to get them back.",
                        self.tag_name(),
                        self.tag_name()
                    )
                } else {
                    format!("Deleted {}, your answers about it and its model. Its stories are back in their folders.", self.tag_name())
                });
                self.tag = None;
                self.draft = None;
                self.undo.clear();
                self.current = None;
                self.pending = None;
                self.queue.clear();
                self.candidates.clear();
                self.load()
            }
            Message::ToggleFolder(folder, on) => {
                let Some(tag) = self.tag else { return Task::none() };
                if on {
                    self.folder_tags.push((folder, tag));
                } else {
                    self.folder_tags.retain(|&ft| ft != (folder, tag));
                }
                let archive = self.archive.clone();
                blocking(
                    move || {
                        let conn = archive.lock();
                        store::set_folder_tag(&conn, folder, tag, on)?;
                        // Stories in a folder that no longer uses the tag leave it
                        store::sort_into_tags(&conn)?;
                        Ok(())
                    },
                    Message::FolderToggled,
                )
            }
            Message::FolderToggled(Ok(())) => {
                self.current = None;
                self.pending = None;
                self.queue.clear();
                Task::batch([self.prepare(), self.load_tag()])
            }
            Message::TagLoaded(Ok(data)) => {
                if Some(data.tag) != self.tag {
                    return Task::none();
                }
                self.candidates = data.candidates;
                self.positives = data.positives;
                self.counts = data.counts;
                self.recent = data.recent;
                self.published = data.published;
                let mut tasks = vec![self.arrange_queue()];
                // A model to review with as soon as there are enough answers,
                // kept up to date as more come in
                let trained_on = self.draft.as_ref().map(|(_, n)| *n);
                if trained_on.is_none_or(|n| self.answers() >= n + AUTO_TRAIN_EVERY) {
                    tasks.push(self.train());
                }
                Task::batch(tasks)
            }
            Message::PickOrder(order) => {
                self.order = order;
                self.arrange_queue()
            }
            Message::SearchText(search) => {
                self.search = search;
                Task::none()
            }
            Message::Search => {
                let Some(tag) = self.tag else { return Task::none() };
                let query = self.search.trim().to_string();
                if query.is_empty() {
                    return self.update(Message::ClearSearch);
                }
                let archive = self.archive.clone();
                blocking(move || Ok((query.clone(), archive.search(tag, &query)?)), Message::Searched)
            }
            Message::Searched(Ok((query, found))) => {
                self.searching = Some(query);
                self.queue = found;
                self.current = None;
                self.pending = None;
                self.arrange_queue()
            }
            Message::ClearSearch => {
                self.searching = None;
                self.search.clear();
                self.current = None;
                self.pending = None;
                self.arrange_queue()
            }
            // (Unless another story has been asked for since)
            Message::StoryLoaded(id, _) if self.pending != Some(id) => Task::none(),
            Message::StoryLoaded(_, Ok(Some(story))) => {
                self.pending = None;
                self.current = Some(story);
                Task::none()
            }
            // Gone (deleted in Shreader meanwhile): on to the next
            Message::StoryLoaded(_, Ok(None)) => self.next_story(),
            Message::Answer(is_tag) => {
                let (Some(tag), Some(story)) = (self.tag, self.current.take()) else { return Task::none() };
                self.candidates.retain(|(id, _)| *id != story.id);
                let archive = self.archive.clone();
                let next = self.next_story();
                let save = blocking(move || archive.answer(story.id, tag, is_tag), Message::Answered);
                Task::batch([save, next])
            }
            Message::Answered(Ok(answered)) => {
                self.undo.push(answered);
                self.load_tag()
            }
            Message::Skip => {
                if let Some(story) = &self.current {
                    self.skipped.insert(story.id);
                }
                self.next_story()
            }
            Message::Undo => {
                let Some(answered) = self.undo.pop() else { return Task::none() };
                let archive = self.archive.clone();
                blocking(
                    move || {
                        archive.undo(&answered)?;
                        Ok(answered.story)
                    },
                    Message::Undone,
                )
            }
            Message::Undone(Ok(story)) => {
                self.front = Some(story);
                self.load_tag()
            }
            Message::Review(story) => {
                if let Some(showing) = self.showing() {
                    self.queue.insert(0, showing);
                }
                self.queue.retain(|&id| id != story);
                self.queue.insert(0, story);
                self.next_story()
            }
            Message::Train => self.train(),
            Message::Trained(result) => {
                self.training = false;
                match result {
                    Ok(trained) => {
                        self.draft = Some(trained);
                        self.draft_in_use = false;
                        self.arrange_queue()
                    }
                    Err(e) => {
                        self.notice = Some(format!("Training didn't work: {}", e));
                        Task::none()
                    }
                }
            }
            Message::Publish => {
                let (Some(tag), Some((trained, _))) = (self.tag, &self.draft) else { return Task::none() };
                let (model, evaluation) = (trained.model.clone(), trained.evaluations[0].clone());
                let archive = self.archive.clone();
                blocking(
                    move || {
                        let conn = archive.lock();
                        store::save_model(&conn, tag, &model, &evaluation)?;
                        store::sort_into_tags(&conn)?;
                        Ok(())
                    },
                    Message::Published,
                )
            }
            Message::Published(Ok(())) => {
                self.draft_in_use = true;
                self.notice = Some(format!("The new {} model is in use. Shreader sorts stories with it from its next refresh.", self.tag_name()));
                self.load_tag()
            }
            Message::DismissNotice => {
                self.notice = None;
                Task::none()
            }
            Message::Loaded(Err(e))
            | Message::TagCreated(Err(e))
            | Message::TagDeleted(Err(e))
            | Message::FolderToggled(Err(e))
            | Message::TagLoaded(Err(e))
            | Message::Searched(Err(e))
            | Message::StoryLoaded(_, Err(e))
            | Message::Answered(Err(e))
            | Message::Undone(Err(e))
            | Message::Published(Err(e)) => {
                self.notice = Some(e);
                Task::none()
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::on_key_press(|key, modifiers| {
            if modifiers.control() || modifiers.alt() || modifiers.logo() {
                return None;
            }
            match key.as_ref() {
                keyboard::Key::Character("y") => Some(Message::Answer(true)),
                keyboard::Key::Character("n") => Some(Message::Answer(false)),
                keyboard::Key::Character("s") => Some(Message::Skip),
                keyboard::Key::Character("u") => Some(Message::Undo),
                _ => None,
            }
        })
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }

    pub fn view(&self) -> Element<'_, Message> {
        let sidebar = container(scrollable(self.sidebar().padding(16)).height(Length::Fill)).width(340).height(Length::Fill);
        let main = column![].push_maybe(self.notice()).push(self.review()).spacing(12).padding(16);
        row![sidebar, iced::widget::vertical_rule(1), main].into()
    }

    fn notice(&self) -> Option<Element<'_, Message>> {
        let notice = self.notice.as_ref()?;
        Some(
            container(
                row![text(notice).size(14).width(Length::Fill), button(text("Dismiss").size(13)).on_press(Message::DismissNotice)]
                    .spacing(12)
                    .align_y(Alignment::Center),
            )
            .padding(10)
            .style(container::bordered_box)
            .width(Length::Fill)
            .into(),
        )
    }

    fn sidebar(&self) -> iced::widget::Column<'_, Message> {
        let heading = |s: &'static str| text(s).size(13).style(text::secondary);
        let choices: Vec<TagChoice> = self.tags.iter().cloned().map(TagChoice).collect();
        let selected = self.tags.iter().find(|t| Some(t.id) == self.tag).cloned().map(TagChoice);
        let tag_row: Element<'_, Message> = match &self.new_tag {
            Some(name) => row![
                text_input("New tag's name", name)
                    .id("new-tag")
                    .on_input(Message::NewTagName)
                    .on_submit(Message::CreateTag)
                    .width(Length::Fill),
                button(text("Add")).on_press_maybe((!name.trim().is_empty()).then_some(Message::CreateTag)),
                button(text("Cancel")).style(button::secondary).on_press(Message::CancelNewTag),
            ]
            .spacing(6)
            .into(),
            None => row![
                pick_list(choices, selected, Message::PickTag).placeholder("No tags yet").width(Length::Fill),
                button(text("New tag…")).style(button::secondary).on_press(Message::StartNewTag),
            ]
            .spacing(6)
            .into(),
        };
        let mut side = column![heading("TAG"), tag_row].spacing(8);
        let Some(tag) = self.tag else {
            return side.push(text("Add a tag (such as Promotions) to start.").size(14));
        };

        side = side.push(Space::with_height(8)).push(heading("SORTS STORIES IN"));
        if self.folders.is_empty() {
            side = side.push(text("No folders yet").size(14));
        }
        for folder in &self.folders {
            let on = self.folder_tags.contains(&(folder.id, tag));
            side = side.push(checkbox(&folder.name, on).on_toggle(move |on| Message::ToggleFolder(folder.id, on)).size(16).text_size(14));
        }

        side = side.push(if self.confirm_delete {
            Element::from(
                column![
                    text(format!(
                        "Delete {}? Its stories go back to their folders (no stories are deleted).",
                        self.tag_name()
                    ))
                    .size(13),
                    button(text("Delete the tag, keep its answers and model").size(13))
                        .width(Length::Fill)
                        .style(button::secondary)
                        .on_press(Message::DeleteTag(true)),
                    text(format!("Making a tag called {} again brings them back.", self.tag_name())).size(12).style(text::secondary),
                    button(text("Delete the tag, its answers and its model").size(13))
                        .width(Length::Fill)
                        .style(button::danger)
                        .on_press(Message::DeleteTag(false)),
                    text("For good: a new tag would start from nothing.").size(12).style(text::secondary),
                    button(text("Cancel").size(13)).style(button::text).on_press(Message::CancelDeleteTag),
                ]
                .spacing(6),
            )
        } else {
            button(text("Delete this tag…").size(13).style(text::secondary))
                .style(button::text)
                .padding([2, 0])
                .on_press(Message::AskDeleteTag)
                .into()
        });

        side = side
            .push(Space::with_height(8))
            .push(heading("REVIEW"))
            .push(pick_list(Order::ALL, Some(self.order), Message::PickOrder).width(Length::Fill))
            .push(
                row![
                    text_input("Search, e.g. promo code", &self.search)
                        .on_input(Message::SearchText)
                        .on_submit(Message::Search)
                        .width(Length::Fill),
                ]
                .push_maybe(self.searching.as_ref().map(|_| button(text("Clear")).style(button::secondary).on_press(Message::ClearSearch)))
                .spacing(6),
            );

        side = side.push(Space::with_height(8)).push(heading("MODEL")).push(self.model_panel());

        if !self.recent.is_empty() {
            side = side.push(Space::with_height(8)).push(heading("LATEST ANSWERS (CLICK TO CHANGE)"));
            for (id, title, is_tag) in &self.recent {
                let mark = if *is_tag { text("In").style(text::success) } else { text("Not").style(text::danger) };
                side = side.push(
                    button(row![mark.size(13).width(30), text(title).size(13)].spacing(4))
                        .style(button::text)
                        .padding([2, 0])
                        .on_press(Message::Review(*id)),
                );
            }
        }
        side
    }

    fn model_panel(&self) -> Element<'_, Message> {
        let name = self.tag_name();
        let mut panel = column![text(format!(
            "{} in {}, {} not{}",
            self.counts.yes,
            name,
            self.counts.no,
            match self.counts.from_reader {
                0 => String::new(),
                n => format!(" ({} corrected in Shreader)", n),
            }
        ))
        .size(14)]
        .spacing(8);
        if !self.can_train() {
            panel = panel.push(
                text(format!("Training needs at least {} answers of each kind.", MIN_EACH)).size(13).style(text::secondary),
            );
        }
        let train_label = if self.training { "Training…" } else { "Train now" };
        panel = panel.push(
            button(text(train_label)).style(button::secondary).on_press_maybe((self.can_train() && !self.training).then_some(Message::Train)),
        );
        if let Some((trained, answers)) = &self.draft {
            let mut lines = column![text(format!("Latest training ({} answers), tried on answers it didn't learn from:", answers)).size(13)]
                .spacing(4);
            for (n, e) in trained.evaluations.iter().enumerate() {
                let kept = if n == 0 { " (kept)" } else { "" };
                let style: fn(&Theme) -> text::Style = if n == 0 { text::default } else { text::secondary };
                lines = lines.push(text(format!("{}{}: {}", capitalised(e.kind.name()), kept, e.describe())).size(13).style(style));
            }
            panel = panel.push(lines);
            let precision = trained.evaluations[0].precision();
            if self.draft_in_use {
            } else if precision >= MIN_PRECISION {
                panel = panel.push(button(text("Use this model in Shreader")).on_press(Message::Publish));
            } else {
                panel = panel.push(
                    text(format!(
                        "Not ready for use: {:.0}% of the stories it tagged should have been, and it needs {:.0}%. More answers will help.",
                        precision * 100.0,
                        MIN_PRECISION * 100.0
                    ))
                    .size(13)
                    .style(text::danger),
                );
            }
        }
        let in_use = match &self.published {
            Some((_, summary, at)) => format!("In use since {}: {}", local_time(at), summary),
            None => "No model in use yet: Shreader only sorts the stories you've answered about.".to_string(),
        };
        panel.push(text(in_use).size(13).style(text::secondary)).into()
    }

    fn review(&self) -> Element<'_, Message> {
        let Some(tag) = self.tag else { return Space::new(Length::Fill, Length::Fill).into() };
        let name = self.tag_name();
        let Some(story) = &self.current else {
            let message = if self.preparing {
                "Preparing stories for review…".to_string()
            } else if !self.folder_tags.iter().any(|&(_, t)| t == tag) {
                format!("Choose the folders {} sorts, on the left, to review their stories.", name)
            } else if self.searching.is_some() {
                "Nothing more matches the search.".to_string()
            } else {
                "Nothing left to review.".to_string()
            };
            return container(text(message).size(16).style(text::secondary)).center(Length::Fill).into();
        };

        let guess = self.guide().zip(self.candidates_embedding(story.id)).map(|(model, e)| {
            let score = model.score(e);
            let verdict = if score >= model.threshold { "it would tag it" } else { "it wouldn't tag it" };
            format!(
                "Model: {:.0}% likely in {} ({}; it tags from {:.0}%)",
                score * 100.0,
                name,
                verdict,
                model.threshold * 100.0
            )
        });
        let date = story.date.as_deref().map(|d| d.replace('T', " ").chars().take(16).collect::<String>()).unwrap_or_default();
        let place = match &self.searching {
            Some(query) => format!("Search \"{}\": {} more", query, self.queue.len()),
            None => format!("{} more to review", self.queue.len()),
        };
        let card = column![
            text(place).size(13).style(text::secondary),
            text(&story.title).size(22),
            text(format!("{} · {}", story.feed, date)).size(14).style(text::secondary),
        ]
        .push_maybe(story.url.as_ref().map(|u| text(u).size(12).style(text::secondary)))
        .push(horizontal_rule(1))
        .push(scrollable(container(text(&story.text).size(15)).padding(iced::Padding::ZERO.right(16))).height(Length::Fill))
        .push(horizontal_rule(1))
        .push_maybe(guess.map(|g| text(g).size(14)))
        .push(
            row![
                button(text(format!("In {}  (Y)", name))).style(button::success).padding([8, 14]).on_press(Message::Answer(true)),
                button(text(format!("Not in {}  (N)", name))).style(button::danger).padding([8, 14]).on_press(Message::Answer(false)),
                button(text("Skip  (S)")).style(button::secondary).padding([8, 14]).on_press(Message::Skip),
                Space::with_width(Length::Fill),
                button(text("Undo  (U)"))
                    .style(button::secondary)
                    .padding([8, 14])
                    .on_press_maybe((!self.undo.is_empty()).then_some(Message::Undo)),
            ]
            .spacing(10),
        )
        .spacing(10);
        card.into()
    }

    fn candidates_embedding(&self, id: i64) -> Option<&[f32]> {
        self.candidates.iter().find(|(c, _)| *c == id).map(|(_, e)| &e[..])
    }
}

fn capitalised(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Downloads the language model's files (see `embedder::ensure_model`)
async fn download(url: String, max_bytes: usize) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    if response.content_length().is_some_and(|n| n as usize > max_bytes) {
        return Err("larger than expected".to_string());
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > max_bytes {
        return Err("larger than expected".to_string());
    }
    Ok(bytes.to_vec())
}

/// A time SQLite recorded (`datetime('now')`: UTC, "2026-11-20 20:24:29"),
/// in the computer's time zone, to the minute. As stored if it doesn't parse.
fn local_time(utc: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(utc, "%Y-%m-%d %H:%M:%S")
        .map(|t| t.and_utc().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| utc.to_string())
}
