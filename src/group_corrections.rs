//! Correcting story groups while reading (from an open group or story in
//! the main view): taking a story out of its group, putting a story or a
//! group into another group, or confirming a group as right. Each changes
//! the groups at once and records the user's answers about those pairs of
//! stories (see `training`). They count for more than Ollama's when the
//! grouping model is retrained, and stand whatever the model thinks.

use std::collections::HashSet;

use crate::db::Database;
use crate::models::Article;
use crate::training::SOURCE_USER;

fn pair(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

/// A correction, as it can be undone: stories' previous groups and the
/// previous labels of the pairs it labelled
#[derive(Debug, Clone)]
pub struct Correction {
    groups: Vec<(i64, i64)>,
    labels: Vec<((i64, i64), Option<bool>)>,
}

/// Takes `story` out of `group`, whose stories are `members` (all of them,
/// `story` included, oldest last): it's recorded as a different story from
/// each of the others. Returns the group the others are now in (a group is
/// known by one of its stories' ids: if it was `story`'s, the rest take the
/// oldest remaining story's), and how to undo it.
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
    Ok((remaining, Correction { groups: previous, labels: previous_labels }))
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
    Ok(Correction { groups: previous, labels: previous_labels })
}

/// Records every pair of `members` as the same story.
pub fn confirm_group(db: &Database, members: &[i64]) -> Result<Correction, String> {
    let labels: Vec<((i64, i64), Option<bool>)> = members
        .iter()
        .enumerate()
        .flat_map(|(n, &a)| members[n + 1..].iter().map(move |&b| (pair(a, b), Some(true))))
        .collect();
    let previous_labels = db.set_pair_labels(&labels, SOURCE_USER).map_err(|e| e.to_string())?;
    Ok(Correction { groups: Vec::new(), labels: previous_labels })
}

/// Undoes a correction: stories go back to their groups, and the pairs it
/// labelled to what they were.
pub fn undo_correction(db: &Database, correction: &Correction) -> Result<(), String> {
    db.set_groups(&correction.groups).map_err(|e| e.to_string())?;
    db.set_pair_labels(&correction.labels, SOURCE_USER).map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ArticleFilter;

    /// Five stories, oldest first: 1, 2, 3 grouped (named after 1), 4 and
    /// 5 on their own
    fn setup() -> (Database, Vec<i64>) {
        let db = Database::open_in_memory().unwrap();
        let feed = db.add_or_update_feed("https://example.com/rss", "Example", None, None, None).unwrap();
        let base = chrono::Utc::now() - chrono::Duration::hours(10);
        for n in 1..=5 {
            db.upsert_article(feed, None, &format!("h{}", n), &format!("Story {}", n), None, None, None, None, None,
                Some(base + chrono::Duration::hours(n)), None).unwrap();
        }
        let mut stories = db.get_articles(&ArticleFilter::default()).unwrap();
        stories.reverse();
        let ids: Vec<i64> = stories.iter().map(|a| a.id).collect();
        db.set_groups(&[(ids[0], ids[0]), (ids[1], ids[0]), (ids[2], ids[0]), (ids[3], ids[3]), (ids[4], ids[4])]).unwrap();
        (db, ids)
    }

    /// A group's story ids, newest first
    fn members(db: &Database, group: i64) -> Vec<i64> {
        db.group_members(&ArticleFilter::default(), &[group]).unwrap().iter().map(|a| a.id).collect()
    }

    fn user_label(db: &Database, a: i64, b: i64) -> Option<bool> {
        let (a, b) = pair(a, b);
        db.pair_labels().unwrap().into_iter().find(|l| (l.0, l.1) == (a, b) && l.2 == SOURCE_USER).map(|l| l.3)
    }

    #[test]
    fn removing_a_story_and_undoing_it() {
        let (db, s) = setup();
        let (remaining, correction) = remove_from_group(&db, s[2], s[0], &members(&db, s[0])).unwrap();
        assert_eq!(remaining, s[0]);
        assert_eq!(members(&db, s[0]), [s[1], s[0]]);
        assert_eq!(members(&db, s[2]), [s[2]]);
        assert_eq!(user_label(&db, s[2], s[0]), Some(false));
        assert_eq!(user_label(&db, s[2], s[1]), Some(false));
        undo_correction(&db, &correction).unwrap();
        assert_eq!(members(&db, s[0]), [s[2], s[1], s[0]]);
        assert_eq!(user_label(&db, s[2], s[0]), None);
    }

    #[test]
    fn removing_the_story_a_group_is_named_after() {
        let (db, s) = setup();
        // The others stay together, under the oldest remaining one's id
        let (remaining, correction) = remove_from_group(&db, s[0], s[0], &members(&db, s[0])).unwrap();
        assert_eq!(remaining, s[1]);
        assert_eq!(members(&db, s[1]), [s[2], s[1]]);
        assert_eq!(members(&db, s[0]), [s[0]]);
        undo_correction(&db, &correction).unwrap();
        assert_eq!(members(&db, s[0]), [s[2], s[1], s[0]]);
    }

    #[test]
    fn joining_stories_and_groups() {
        let (db, s) = setup();
        // 5 joins 4, making a group; then that group joins 1's
        let moving = db.group_members(&ArticleFilter::default(), &[s[4]]).unwrap();
        join_groups(&db, &moving, s[3], &members(&db, s[3])).unwrap();
        assert_eq!(members(&db, s[3]), [s[4], s[3]]);
        let moving = db.group_members(&ArticleFilter::default(), &[s[3]]).unwrap();
        let merge = join_groups(&db, &moving, s[0], &members(&db, s[0])).unwrap();
        assert_eq!(members(&db, s[0]).len(), 5);
        // 4 and 5 are the same as each other and each of 1, 2, 3; nothing's
        // recorded as different
        assert_eq!(db.pair_label_counts(SOURCE_USER).unwrap(), (7, 7));
        undo_correction(&db, &merge).unwrap();
        assert_eq!(members(&db, s[0]).len(), 3);
        assert_eq!(members(&db, s[3]), [s[4], s[3]]);
        assert_eq!(db.pair_label_counts(SOURCE_USER).unwrap(), (1, 1));
    }

    #[test]
    fn confirming_a_group() {
        let (db, s) = setup();
        let group = members(&db, s[0]);
        assert!(!all_confirmed(&db, &group));
        let correction = confirm_group(&db, &group).unwrap();
        assert!(all_confirmed(&db, &group));
        assert_eq!(db.pair_label_counts(SOURCE_USER).unwrap(), (3, 3));
        undo_correction(&db, &correction).unwrap();
        assert!(!all_confirmed(&db, &group));
        assert!(!all_confirmed(&db, &[s[3]]), "one story isn't a group");
    }
}
