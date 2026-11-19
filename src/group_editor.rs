//! Correcting story groups by hand: the last week's single stories on the
//! left, its groups on the right. Moving a story into a group, taking it
//! out, merging two groups, or confirming a group as right changes the
//! groups at once and
//! records the user's answers about those pairs of stories (see
//! `training`). They count for more than Ollama's when the grouping model
//! is retrained (on Done), and stand whatever the model thinks.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use iced::widget::{button, column, container, row, scrollable, text, text_input, tooltip};
use iced::{Alignment, Element, Length, Theme};

use crate::db::Database;
use crate::models::Article;
use crate::training::{LABEL_DAYS, SOURCE_USER};

/// Most single stories and groups listed at once (the newest; searching
/// finds the others)
const SHOWN_SINGLES: usize = 150;
const SHOWN_GROUPS: usize = 80;

pub struct Story {
    pub id: i64,
    pub title: String,
    pub feed: String,
    pub date: DateTime<Utc>,
    /// The start of its text, shown when hovering its title
    pub snippet: String,
    pub group: i64,
}

/// One change, as it can be undone: stories' previous groups and the
/// previous labels of the pairs it labelled
#[derive(Debug, Clone)]
struct Change {
    groups: Vec<(i64, i64)>,
    labels: Vec<((i64, i64), Option<bool>)>,
}

#[derive(Debug, Clone)]
pub enum EditorMessage {
    Search(String),
    /// Choose a story to move
    Pick(i64),
    /// Choose a group to merge into another
    PickGroup(i64),
    CancelPick,
    /// Move the chosen story, or merge the chosen group, into this group
    MoveTo(i64),
    /// Take a story out of its group
    Remove(i64),
    /// Mark a group as right as it is
    Confirm(i64),
    /// Open or close a group
    Toggle(i64),
    Undo,
    Done,
}

pub struct GroupEditor {
    /// Newest first
    stories: Vec<Story>,
    index: HashMap<i64, usize>,
    /// Each group's stories (indexes into `stories`), newest first
    members: HashMap<i64, Vec<usize>>,
    /// Pairs the user has said are the same story
    same_pairs: HashSet<(i64, i64)>,
    query: String,
    picked: Option<Picked>,
    expanded: HashSet<i64>,
    undo: Vec<Change>,
    /// Anything was changed (the model is retrained on Done)
    pub changed: bool,
}

/// What's being moved
#[derive(Debug, Clone, Copy, PartialEq)]
enum Picked {
    Story(i64),
    Group(i64),
}

fn pair(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

/// A correction made outside this view (in an open group), as it can be
/// undone
#[derive(Debug, Clone)]
pub struct Correction(Change);

/// Takes `story` out of its group, whose stories are `members` (all of
/// them, `story` included, oldest last): it's recorded as a different story
/// from each of the others. Returns the group the others are now in (if it
/// was named after `story`, it takes the oldest remaining story's id), and
/// how to undo it.
pub fn remove_from_group(db: &Database, story: i64, group: i64, members: &[i64]) -> Result<(i64, Correction), String> {
    let others: Vec<i64> = members.iter().copied().filter(|&id| id != story).collect();
    let mut groups = vec![(story, story)];
    let mut previous = vec![(story, group)];
    let mut remaining = group;
    if group == story {
        if let Some(&oldest) = others.last() {
            remaining = oldest;
            groups.extend(others.iter().map(|&id| (id, oldest)));
            previous.extend(others.iter().map(|&id| (id, group)));
        }
    }
    let labels: Vec<((i64, i64), Option<bool>)> = others.iter().map(|&other| (pair(story, other), Some(false))).collect();
    db.set_groups(&groups).map_err(|e| e.to_string())?;
    let previous_labels = db.set_pair_labels(&labels, SOURCE_USER).map_err(|e| e.to_string())?;
    Ok((remaining, Correction(Change { groups: previous, labels: previous_labels })))
}

/// Puts the stories `moving` (a group, or one story) into group `target`,
/// whose stories are `staying`: each of one is recorded as the same story
/// as each of the other.
pub fn join_groups(db: &Database, moving: &[Article], target: i64, staying: &[i64]) -> Result<Correction, String> {
    let groups: Vec<(i64, i64)> = moving.iter().map(|a| (a.id, target)).collect();
    let previous: Vec<(i64, i64)> = moving.iter().map(|a| (a.id, a.group())).collect();
    let labels: Vec<((i64, i64), Option<bool>)> = moving
        .iter()
        .flat_map(|a| staying.iter().map(move |&b| (pair(a.id, b), Some(true))))
        .collect();
    db.set_groups(&groups).map_err(|e| e.to_string())?;
    let previous_labels = db.set_pair_labels(&labels, SOURCE_USER).map_err(|e| e.to_string())?;
    Ok(Correction(Change { groups: previous, labels: previous_labels }))
}

/// Records every pair of `members` as the same story.
pub fn confirm_group(db: &Database, members: &[i64]) -> Result<Correction, String> {
    let labels: Vec<((i64, i64), Option<bool>)> = members
        .iter()
        .enumerate()
        .flat_map(|(n, &a)| members[n + 1..].iter().map(move |&b| (pair(a, b), Some(true))))
        .collect();
    let previous_labels = db.set_pair_labels(&labels, SOURCE_USER).map_err(|e| e.to_string())?;
    Ok(Correction(Change { groups: Vec::new(), labels: previous_labels }))
}

/// Undoes a correction: stories go back to their groups, and the pairs it
/// labelled to what they were.
pub fn undo_correction(db: &Database, correction: &Correction) -> Result<(), String> {
    db.set_groups(&correction.0.groups).map_err(|e| e.to_string())?;
    db.set_pair_labels(&correction.0.labels, SOURCE_USER).map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether the user has said every pair of `members` is the same story
pub fn all_confirmed(db: &Database, members: &[i64]) -> bool {
    if members.len() < 2 {
        return false;
    }
    let ids: HashSet<i64> = members.iter().copied().collect();
    let same: HashSet<(i64, i64)> = db
        .pair_labels()
        .unwrap_or_default()
        .into_iter()
        .filter(|(a, b, source, same)| source == SOURCE_USER && *same && ids.contains(a) && ids.contains(b))
        .map(|(a, b, _, _)| (a, b))
        .collect();
    members.iter().enumerate().all(|(n, &a)| members[n + 1..].iter().all(|&b| same.contains(&pair(a, b))))
}

impl GroupEditor {
    /// The last `LABEL_DAYS` days' stories and their groups.
    pub fn load(db: &Database) -> anyhow::Result<Self> {
        let since = Utc::now() - chrono::Duration::days(LABEL_DAYS);
        let mut stories: Vec<Story> = db
            .stories_for_correcting(&since.to_rfc3339())?
            .into_iter()
            .map(|(id, title, feed, date, text, group)| Story {
                id,
                title,
                feed,
                date,
                snippet: text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect(),
                group: group.unwrap_or(id),
            })
            .collect();
        stories.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));
        let same_pairs = db
            .pair_labels()?
            .into_iter()
            .filter(|(_, _, source, same)| source == SOURCE_USER && *same)
            .map(|(a, b, _, _)| (a, b))
            .collect();
        let mut editor = Self {
            stories,
            index: HashMap::new(),
            members: HashMap::new(),
            same_pairs,
            query: String::new(),
            picked: None,
            expanded: HashSet::new(),
            undo: Vec::new(),
            changed: false,
        };
        editor.rebuild();
        Ok(editor)
    }

    fn rebuild(&mut self) {
        self.index = self.stories.iter().enumerate().map(|(i, s)| (s.id, i)).collect();
        self.members.clear();
        for (i, story) in self.stories.iter().enumerate() {
            self.members.entry(story.group).or_default().push(i);
        }
    }

    fn group_of(&self, story: i64) -> Option<i64> {
        self.index.get(&story).map(|&i| self.stories[i].group)
    }

    /// The ids of a group's stories
    fn member_ids(&self, group: i64) -> Vec<i64> {
        self.members.get(&group).map(|m| m.iter().map(|&i| self.stories[i].id).collect()).unwrap_or_default()
    }

    /// Whether the user has said every pair in the group is the same story
    fn confirmed(&self, group: i64) -> bool {
        let ids = self.member_ids(group);
        ids.iter().enumerate().all(|(n, &a)| ids[n + 1..].iter().all(|&b| self.same_pairs.contains(&pair(a, b))))
    }

    /// Acts on a message (all but `Done`, which the app handles).
    pub fn update(&mut self, db: &Database, message: EditorMessage) -> Result<(), String> {
        match message {
            EditorMessage::Search(query) => self.query = query,
            EditorMessage::Pick(story) => self.picked = Some(Picked::Story(story)),
            EditorMessage::PickGroup(group) => self.picked = Some(Picked::Group(group)),
            EditorMessage::CancelPick => self.picked = None,
            EditorMessage::Toggle(group) => {
                if !self.expanded.remove(&group) {
                    self.expanded.insert(group);
                }
            }
            EditorMessage::MoveTo(group) => {
                match self.picked.take() {
                    Some(Picked::Story(story)) => self.move_story(db, story, Some(group))?,
                    Some(Picked::Group(from)) => self.merge(db, from, group)?,
                    None => return Ok(()),
                }
                self.expanded.insert(group);
            }
            EditorMessage::Remove(story) => {
                self.picked = None;
                self.move_story(db, story, None)?;
            }
            EditorMessage::Confirm(group) => {
                let ids = self.member_ids(group);
                let labels: Vec<((i64, i64), Option<bool>)> = ids
                    .iter()
                    .enumerate()
                    .flat_map(|(n, &a)| ids[n + 1..].iter().map(move |&b| (pair(a, b), Some(true))))
                    .collect();
                self.apply(db, Vec::new(), labels)?;
            }
            EditorMessage::Undo => {
                self.picked = None;
                if let Some(change) = self.undo.pop() {
                    db.set_groups(&change.groups).map_err(|e| e.to_string())?;
                    db.set_pair_labels(&change.labels, SOURCE_USER).map_err(|e| e.to_string())?;
                    self.record(&change.groups, &change.labels);
                }
            }
            EditorMessage::Done => {}
        }
        Ok(())
    }

    /// Moves `story` into `target`'s group, or (`None`) on its own: it's
    /// recorded as a different story from the rest of its old group, and
    /// the same story as everything in the new one.
    fn move_story(&mut self, db: &Database, story: i64, target: Option<i64>) -> Result<(), String> {
        let Some(old) = self.group_of(story) else {
            return Ok(());
        };
        let others: Vec<i64> = self.member_ids(old).into_iter().filter(|&id| id != story).collect();
        if target == Some(old) || (target.is_none() && others.is_empty()) {
            return Ok(());
        }
        let mut groups = Vec::new();
        // A group is known by one of its stories' ids: if it's this story's,
        // the rest of the group takes another's (its oldest)
        if old == story {
            if let Some(&oldest) = others.last() {
                groups.extend(others.iter().map(|&id| (id, oldest)));
            }
        }
        let new_group = target.unwrap_or(story);
        groups.push((story, new_group));
        let mut labels: Vec<((i64, i64), Option<bool>)> =
            others.iter().map(|&other| (pair(story, other), Some(false))).collect();
        if let Some(target) = target {
            labels.extend(self.member_ids(target).into_iter().map(|other| (pair(story, other), Some(true))));
        }
        self.apply(db, groups, labels)
    }

    /// Merges group `from` into group `into`: every story of one is recorded
    /// as the same story as every story of the other.
    fn merge(&mut self, db: &Database, from: i64, into: i64) -> Result<(), String> {
        if from == into {
            return Ok(());
        }
        let (moving, staying) = (self.member_ids(from), self.member_ids(into));
        let groups: Vec<(i64, i64)> = moving.iter().map(|&id| (id, into)).collect();
        let labels: Vec<((i64, i64), Option<bool>)> = moving
            .iter()
            .flat_map(|&a| staying.iter().map(move |&b| (pair(a, b), Some(true))))
            .collect();
        self.expanded.remove(&from);
        self.apply(db, groups, labels)
    }

    /// Makes a change, in the archive and here, keeping a way to undo it.
    fn apply(
        &mut self,
        db: &Database,
        groups: Vec<(i64, i64)>,
        labels: Vec<((i64, i64), Option<bool>)>,
    ) -> Result<(), String> {
        let previous_groups: Vec<(i64, i64)> =
            groups.iter().filter_map(|&(story, _)| Some((story, self.group_of(story)?))).collect();
        db.set_groups(&groups).map_err(|e| e.to_string())?;
        let previous_labels = db.set_pair_labels(&labels, SOURCE_USER).map_err(|e| e.to_string())?;
        self.record(&groups, &labels);
        self.undo.push(Change { groups: previous_groups, labels: previous_labels });
        self.changed = true;
        Ok(())
    }

    /// Brings this view up to date with a change made in the archive.
    fn record(&mut self, groups: &[(i64, i64)], labels: &[((i64, i64), Option<bool>)]) {
        for &(story, group) in groups {
            if let Some(&i) = self.index.get(&story) {
                self.stories[i].group = group;
            }
        }
        for &(pair, same) in labels {
            if same == Some(true) {
                self.same_pairs.insert(pair);
            } else {
                self.same_pairs.remove(&pair);
            }
        }
        self.rebuild();
    }

    fn matches(&self, story: &Story) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty() || story.title.to_lowercase().contains(&query) || story.feed.to_lowercase().contains(&query)
    }

    pub fn view(&self) -> Element<'_, EditorMessage> {
        let header = row![
            column![
                text("Correct story groups").size(18),
                text(format!(
                    "Stories from the last {} days. Your corrections teach the grouping; Done applies what it learned to recent stories.",
                    LABEL_DAYS
                ))
                .size(12)
                .style(text::secondary),
            ]
            .spacing(2)
            .width(Length::Fill),
            text_input("Search titles or feeds…", &self.query)
                .on_input(EditorMessage::Search)
                .size(13)
                .padding([5, 8])
                .width(260),
            button(text("Undo").size(13))
                .padding([5, 12])
                .style(button::secondary)
                .on_press_maybe((!self.undo.is_empty()).then_some(EditorMessage::Undo)),
            button(text("Done").size(13)).padding([5, 14]).on_press(EditorMessage::Done),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        // What to do next (always there, so nothing below it jumps)
        let picked_title = |id: i64| self.index.get(&id).map(|&i| self.stories[i].title.as_str()).unwrap_or("");
        let hint = match self.picked {
            Some(Picked::Story(story)) => Some(format!(
                "Moving “{}”: choose Move here on the group it belongs in, or Group with this on a single story.",
                picked_title(story)
            )),
            Some(Picked::Group(group)) => {
                let first = self.members.get(&group).and_then(|m| m.last()).map(|&i| self.stories[i].id).unwrap_or(group);
                Some(format!(
                    "Merging “{}” ({} stories): choose Merge here on the group about the same news.",
                    picked_title(first),
                    self.member_ids(group).len()
                ))
            }
            None => None,
        };
        let picked_bar = container(match hint {
            Some(hint) => row![
                text(hint).size(13).width(Length::Fill),
                button(text("Cancel").size(12)).padding([3, 10]).style(button::secondary).on_press(EditorMessage::CancelPick),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            None => row![text(
                "Move… puts a story in the right group (or on its own, with Remove). Merge… joins two groups about the same news. Looks right confirms a group."
            )
            .size(12)
            .style(text::secondary)
            .width(Length::Fill)]
            .align_y(Alignment::Center),
        })
        .padding([6, 10])
        .height(36)
        .align_y(iced::alignment::Vertical::Center)
        .style(if self.picked.is_some() { picked_style } else { story_style });

        // Single stories (newest first)
        let singles: Vec<&Story> = self
            .stories
            .iter()
            .filter(|s| self.members.get(&s.group).is_some_and(|m| m.len() == 1) && self.matches(s))
            .collect();
        let mut single_rows = column![].spacing(4);
        for story in singles.iter().take(SHOWN_SINGLES) {
            single_rows = single_rows.push(self.story_row(story, false));
        }
        let left = column![
            column_heading("Not grouped", singles.len(), SHOWN_SINGLES),
            scrollable(container(single_rows).padding(iced::Padding { right: 12.0, ..Default::default() }))
                .height(Length::Fill),
        ]
        .spacing(8)
        .width(Length::FillPortion(2));

        // Groups (with the newest stories first)
        let mut groups: Vec<(i64, &Vec<usize>)> = self
            .members
            .iter()
            .filter(|(_, m)| m.len() > 1 && m.iter().any(|&i| self.matches(&self.stories[i])))
            .map(|(g, m)| (*g, m))
            .collect();
        groups.sort_by(|a, b| self.stories[a.1[0]].date.cmp(&self.stories[b.1[0]].date).reverse().then(b.0.cmp(&a.0)));
        let mut group_cards = column![].spacing(6);
        for &(group, members) in groups.iter().take(SHOWN_GROUPS) {
            group_cards = group_cards.push(self.group_card(group, members));
        }
        let right = column![
            column_heading("Groups", groups.len(), SHOWN_GROUPS),
            scrollable(container(group_cards).padding(iced::Padding { right: 12.0, ..Default::default() }))
                .height(Length::Fill),
        ]
        .spacing(8)
        .width(Length::FillPortion(3));

        container(
            column![header, picked_bar, row![left, right].spacing(16).height(Length::Fill)]
                .spacing(10)
                .height(Length::Fill),
        )
        .padding(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn group_card<'a>(&'a self, group: i64, members: &'a [usize]) -> Element<'a, EditorMessage> {
        let open = self.expanded.contains(&group);
        // Named after its first story (the oldest)
        let first = &self.stories[*members.last().expect("groups have stories")];
        let mut actions = row![].spacing(6).align_y(Alignment::Center);
        let is_picked = self.picked == Some(Picked::Group(group));
        match self.picked {
            Some(Picked::Story(story)) if self.group_of(story) != Some(group) => {
                actions = actions.push(
                    button(text("Move here").size(12)).padding([3, 10]).on_press(EditorMessage::MoveTo(group)),
                );
            }
            Some(Picked::Group(other)) if other != group => {
                actions = actions.push(
                    button(text("Merge here").size(12)).padding([3, 10]).on_press(EditorMessage::MoveTo(group)),
                );
            }
            Some(Picked::Group(_)) => {
                actions = actions.push(
                    button(text("Cancel").size(12))
                        .padding([3, 10])
                        .style(button::secondary)
                        .on_press(EditorMessage::CancelPick),
                );
            }
            Some(Picked::Story(_)) => {}
            None => {
                actions = actions.push(
                    button(text("Merge…").size(12))
                        .padding([3, 10])
                        .style(button::secondary)
                        .on_press(EditorMessage::PickGroup(group)),
                );
            }
        }
        actions = actions.push(if self.confirmed(group) {
            Element::from(text("Confirmed").size(12).style(text::success))
        } else {
            button(text("Looks right").size(12))
                .padding([3, 10])
                .style(button::secondary)
                .on_press(EditorMessage::Confirm(group))
                .into()
        });
        let heading = button(
            row![
                text(if open { "▾" } else { "▸" }).size(13),
                column![
                    text(&first.title).size(13),
                    text(format!("{} stories", members.len())).size(11).style(text::secondary),
                ]
                .spacing(2),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([2, 4])
        .width(Length::Fill)
        .style(button::text)
        .on_press(EditorMessage::Toggle(group));
        let mut card = column![row![heading, actions].spacing(8).align_y(Alignment::Center)].spacing(4);
        if open {
            for &i in members {
                card = card.push(self.story_row(&self.stories[i], true));
            }
        }
        container(card)
            .padding([6, 8])
            .width(Length::Fill)
            .style(move |theme: &Theme| {
                let mut style = container::bordered_box(theme);
                if is_picked {
                    style.border = picked_style(theme).border;
                }
                style
            })
            .into()
    }

    fn story_row<'a>(&'a self, story: &'a Story, in_group: bool) -> Element<'a, EditorMessage> {
        let title = text(&story.title).size(13);
        let title: Element<'_, EditorMessage> = if story.snippet.is_empty() {
            title.into()
        } else {
            tooltip(
                title,
                container(text(&story.snippet).size(12)).padding(8).max_width(420).style(container::bordered_box),
                tooltip::Position::Bottom,
            )
            .into()
        };
        let details = column![
            title,
            text(format!("{} • {}", story.feed, crate::app::format_date(story.date))).size(11).style(text::secondary),
        ]
        .spacing(2)
        .width(Length::Fill);

        let small = |label: &'static str, message: EditorMessage| {
            button(text(label).size(12)).padding([3, 10]).style(button::secondary).on_press(message)
        };
        let is_picked = self.picked == Some(Picked::Story(story.id));
        let mut actions = row![].spacing(6).align_y(Alignment::Center);
        if is_picked {
            actions = actions.push(small("Cancel", EditorMessage::CancelPick));
        } else if matches!(self.picked, Some(Picked::Story(_))) && !in_group {
            actions = actions.push(
                button(text("Group with this").size(12)).padding([3, 10]).on_press(EditorMessage::MoveTo(story.group)),
            );
        } else if self.picked.is_none() {
            actions = actions.push(small("Move…", EditorMessage::Pick(story.id)));
            if in_group {
                actions = actions.push(small("Remove", EditorMessage::Remove(story.id)));
            }
        }
        container(row![details, actions].spacing(8).align_y(Alignment::Center))
            .padding([6, 10])
            .width(Length::Fill)
            .style(move |theme: &Theme| if is_picked { picked_style(theme) } else { story_style(theme) })
            .into()
    }
}

fn column_heading<'a>(name: &str, count: usize, shown: usize) -> Element<'a, EditorMessage> {
    let label = if count > shown {
        format!("{} ({}, newest {} shown; search for others)", name, count, shown)
    } else {
        format!("{} ({})", name, count)
    };
    text(label).size(14).into()
}

fn story_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        border: iced::Border { color: palette.background.strong.color, width: 1.0, radius: 4.0.into() },
        ..container::Style::default()
    }
}

fn picked_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        border: iced::Border { color: palette.primary.strong.color, width: 2.0, radius: 4.0.into() },
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five stories: 1, 2, 3 grouped (as group 1), 4 and 5 on their own
    fn setup() -> (Database, GroupEditor) {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://example.com/rss", "Example", None, None, None).unwrap();
        let base = Utc::now() - chrono::Duration::hours(10);
        for n in 1..=5 {
            db.upsert_article(feed, None, &format!("h{}", n), &format!("Story {}", n), None, None, None, None, None,
                Some(base + chrono::Duration::hours(n)), None).unwrap();
        }
        let ids: Vec<i64> = {
            let mut s = db.stories_for_correcting("2000-01-01T00:00:00+00:00").unwrap();
            s.sort_by_key(|r| r.3);
            s.into_iter().map(|r| r.0).collect()
        };
        db.set_groups(&[(ids[0], ids[0]), (ids[1], ids[0]), (ids[2], ids[0]), (ids[3], ids[3]), (ids[4], ids[4])]).unwrap();
        let editor = GroupEditor::load(&db).unwrap();
        (db, editor)
    }

    fn id(editor: &GroupEditor, title: &str) -> i64 {
        editor.stories.iter().find(|s| s.title == title).unwrap().id
    }

    fn user_label(db: &Database, a: i64, b: i64) -> Option<bool> {
        let (a, b) = pair(a, b);
        db.pair_labels().unwrap().into_iter().find(|l| (l.0, l.1) == (a, b) && l.2 == SOURCE_USER).map(|l| l.3)
    }

    #[test]
    fn moving_a_story_labels_and_regroups() {
        let (db, mut editor) = setup();
        let (s1, s2, s3, s4) = (id(&editor, "Story 1"), id(&editor, "Story 2"), id(&editor, "Story 3"), id(&editor, "Story 4"));
        // Story 3 belongs with story 4, not with 1 and 2
        editor.update(&db, EditorMessage::Pick(s3)).unwrap();
        editor.update(&db, EditorMessage::MoveTo(s4)).unwrap();
        assert_eq!(editor.group_of(s3), Some(s4));
        assert_eq!(user_label(&db, s3, s4), Some(true));
        assert_eq!(user_label(&db, s3, s1), Some(false));
        assert_eq!(user_label(&db, s3, s2), Some(false));
        // Undo puts everything back
        editor.update(&db, EditorMessage::Undo).unwrap();
        assert_eq!(editor.group_of(s3), Some(s1));
        assert_eq!(user_label(&db, s3, s4), None);
        let fresh = GroupEditor::load(&db).unwrap();
        assert_eq!(fresh.group_of(s3), Some(s1));
    }

    #[test]
    fn removing_the_story_a_group_is_named_after() {
        let (db, mut editor) = setup();
        let (s1, s2, s3) = (id(&editor, "Story 1"), id(&editor, "Story 2"), id(&editor, "Story 3"));
        // Group "1" loses story 1: the others stay together, under another id
        editor.update(&db, EditorMessage::Remove(s1)).unwrap();
        assert_eq!(editor.group_of(s1), Some(s1));
        assert_eq!(editor.group_of(s2), editor.group_of(s3));
        assert_ne!(editor.group_of(s2), Some(s1));
        let fresh = GroupEditor::load(&db).unwrap();
        assert_eq!(fresh.group_of(s2), fresh.group_of(s3));
        assert_eq!(fresh.member_ids(s1), [s1]);
    }

    #[test]
    fn merging_groups() {
        let (db, mut editor) = setup();
        let (s1, s4, s5) = (id(&editor, "Story 1"), id(&editor, "Story 4"), id(&editor, "Story 5"));
        // 4 and 5 make a group; then it's merged into 1's
        editor.update(&db, EditorMessage::Pick(s5)).unwrap();
        editor.update(&db, EditorMessage::MoveTo(s4)).unwrap();
        editor.update(&db, EditorMessage::PickGroup(s4)).unwrap();
        editor.update(&db, EditorMessage::MoveTo(s1)).unwrap();
        assert_eq!(editor.member_ids(s1).len(), 5);
        // 4 and 5 are the same as each of 1, 2, 3 (and each other); nothing's
        // recorded as different
        assert_eq!(db.pair_label_counts(SOURCE_USER).unwrap(), (7, 7));
        editor.update(&db, EditorMessage::Undo).unwrap();
        assert_eq!(editor.member_ids(s1).len(), 3);
        assert_eq!(editor.group_of(s5), Some(s4));
    }

    #[test]
    fn confirming_a_group() {
        let (db, mut editor) = setup();
        let s1 = id(&editor, "Story 1");
        assert!(!editor.confirmed(s1));
        editor.update(&db, EditorMessage::Confirm(s1)).unwrap();
        assert!(editor.confirmed(s1));
        assert!(GroupEditor::load(&db).unwrap().confirmed(s1));
        assert_eq!(db.pair_label_counts(SOURCE_USER).unwrap(), (3, 3));
    }
}
