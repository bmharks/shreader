//! Story grouping: stories about the same thing, from any feed, become one
//! group (the same event from several sources, or a site's follow-ups on a
//! developing story).
//!
//! Only folders chosen for it are grouped (from their right-click menu), and
//! each folder's stories are compared only with each other: a folder's
//! groups, and what its grouping learns, are its own.
//!
//! Each story is described by its significant words, from the title and the
//! start of its text: common English words are left out, and words are
//! weighted by how rare they are among the stories being grouped (TF-IDF),
//! so names, places and topics count for much more than everyday words. The
//! title counts double: headlines carry the story.
//!
//! Stories are taken in date order. Each one joins the most similar group
//! that's had a story in the last `WINDOW` (comparing against the group's
//! combined words, its "centroid"), if that similarity reaches `THRESHOLD`;
//! otherwise it starts a group of its own. Comparing with the whole group
//! rather than with any one member stops loosely related stories chaining
//! into one ever-growing group.
//!
//! Everything runs locally and takes milliseconds for thousands of stories.

use std::collections::{HashMap, HashSet};

/// How similar (cosine, 0 to 1) a story must be to a group to join it
pub const THRESHOLD: f32 = 0.30;

/// Significant words a story must share with a group to join it: one
/// uncommon word in common ("Light" in "007 First Light", "Bike Light" and
/// "Light-Day") isn't a shared story
pub const MIN_SHARED_WORDS: usize = 2;

/// How long a group stays open for new stories after its latest one
pub const WINDOW_SECONDS: i64 = 3 * 24 * 60 * 60;

/// How much of a story's text (after the title) describes it
pub const TEXT_CHARS: usize = 400;

/// Stories grouped per run: memory stays small however big the archive
const BATCH: usize = 3000;

/// Settings key of a folder's learned grouping model (see `pair_model`),
/// once it has one
pub fn model_setting(folder_id: i64) -> String {
    format!("grouping_model:{}", folder_id)
}

/// Settings key of what's known about a folder's model (see
/// `training::ModelInfo`)
pub fn model_info_setting(folder_id: i64) -> String {
    format!("grouping_model_info:{}", folder_id)
}

/// One grouping at a time: a refresh's grouping and a regrouping after
/// training would otherwise both pick up the same ungrouped stories
static GROUPING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Groups every story in grouped folders not grouped yet, oldest first, in
/// batches, each folder on its own and with its learned model when it has
/// one. First takes stories out of groups they no longer belong in (see
/// `Database::tidy_groups`). Returns how many were grouped. Run after
/// refreshes (and embedding), and after folders change, off the UI thread.
pub fn group_new_stories(db: &crate::db::Database) -> anyhow::Result<usize> {
    let _one_at_a_time = GROUPING.lock().unwrap_or_else(|p| p.into_inner());
    group_unlocked(db)
}

/// Groups a folder's recent stories again from scratch (after its model is
/// retrained). Older stories keep their groups, and other folders theirs.
pub fn regroup_recent(db: &crate::db::Database, folder: i64) -> anyhow::Result<usize> {
    let _one_at_a_time = GROUPING.lock().unwrap_or_else(|p| p.into_inner());
    let since = chrono::Utc::now() - chrono::Duration::days(crate::embeddings::RECENT_DAYS);
    db.clear_groups_since(&since.to_rfc3339(), folder)?;
    db.tidy_groups()?;
    if !db.grouping_folders()?.contains(&folder) {
        return Ok(0);
    }
    group_folder(db, folder)
}

fn group_unlocked(db: &crate::db::Database) -> anyhow::Result<usize> {
    db.tidy_groups()?;
    let mut grouped = 0;
    for folder in db.grouping_folders()? {
        grouped += group_folder(db, folder)?;
    }
    Ok(grouped)
}

/// Groups a folder's stories not grouped yet, comparing them only with its
/// own stories.
fn group_folder(db: &crate::db::Database, folder: i64) -> anyhow::Result<usize> {
    let model = db
        .get_setting(&model_setting(folder))?
        .and_then(|json| crate::pair_model::PairModel::from_json(&json));
    let user_labels: HashMap<(i64, i64), bool> = match &model {
        Some(_) => db
            .pair_labels_in(folder)?
            .into_iter()
            .filter(|(_, _, source, _)| source == crate::training::SOURCE_USER)
            .map(|(a, b, _, same)| ((a, b), same))
            .collect(),
        None => HashMap::new(),
    };
    let mut grouped = 0;
    loop {
        let stories = db.stories_to_group(folder, BATCH, WINDOW_SECONDS)?;
        let assigned = match &model {
            Some(model) => {
                let ids: Vec<i64> = stories.iter().map(|s| s.id).collect();
                let embeddings = db.get_embeddings(&ids)?;
                let judge = Judge { model, embeddings: &embeddings, user_labels: &user_labels };
                assign_groups_judged(&stories, Some(&judge))
            }
            None => assign_groups(&stories),
        };
        if assigned.is_empty() {
            return Ok(grouped);
        }
        db.set_groups(&assigned)?;
        grouped += assigned.len();
        if assigned.len() < BATCH {
            return Ok(grouped);
        }
    }
}

/// A story to group
#[derive(Debug, Clone)]
pub struct StoryText {
    pub id: i64,
    /// Its feed: words most of a feed's stories share are its template,
    /// not what a story is about (see `feed_boilerplate`)
    pub feed: i64,
    /// Unix time it's sorted by
    pub time: i64,
    pub title: String,
    /// The start of its text
    pub text: String,
    /// Its group, if it already has one (stories grouped in an earlier run,
    /// given as context so new stories can join their groups)
    pub group: Option<i64>,
}

/// Assigns a group to each story that doesn't have one. `stories` holds those
/// plus already-grouped recent stories for context. Returns (story, group)
/// for the newly grouped ones; a story starting its own group gets its own
/// id as the group.
pub fn assign_groups(stories: &[StoryText]) -> Vec<(i64, i64)> {
    assign_groups_judged(stories, None)
}

/// Each story's weighted words (TF-IDF over `stories`, its feed's
/// boilerplate left out, scaled to length 1), in the same order.
pub fn word_vectors(stories: &[StoryText]) -> Vec<HashMap<String, f32>> {
    // Words of each story (less its feed's boilerplate), and how many
    // stories each word appears in
    let mut words: Vec<HashMap<String, f32>> = stories.iter().map(story_words).collect();
    let boilerplate = feed_boilerplate(stories, &words);
    for (story, story_words) in stories.iter().zip(words.iter_mut()) {
        if let Some(feed_words) = boilerplate.get(&story.feed) {
            story_words.retain(|word, _| !feed_words.contains(word));
        }
    }
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for story_words in &words {
        for word in story_words.keys() {
            *document_frequency.entry(word.as_str()).or_default() += 1;
        }
    }
    let total = stories.len() as f32;
    let idf = |word: &str| ((total + 1.0) / (document_frequency.get(word).copied().unwrap_or(0) as f32 + 1.0)).ln() + 1.0;

    words
        .iter()
        .map(|story_words| {
            let mut vector: HashMap<String, f32> =
                story_words.iter().map(|(word, count)| (word.clone(), count * idf(word))).collect();
            normalize(&mut vector);
            vector
        })
        .collect()
}

/// The learned judge of whether two stories are the same news (see
/// `pair_model`), with what it needs. Stories without an embedding are
/// grouped by words alone.
pub struct Judge<'a> {
    pub model: &'a crate::pair_model::PairModel,
    pub embeddings: &'a HashMap<i64, Vec<f32>>,
    /// The user's own answers, which stand whatever the model thinks
    pub user_labels: &'a HashMap<(i64, i64), bool>,
}

/// A story joins a group (by the learned judge) if it's the same news as
/// this many of the group's recent members, on average: as much like the
/// group as a whole as like any one member, so loosely related stories
/// don't chain into one ever-growing group
const LINKAGE: usize = 2;

/// How sure the judge must be that a story belongs with a group to put it there
const JOIN_CHANCE: f32 = 0.5;

/// As `assign_groups`, judging with `judge` when there is one.
pub fn assign_groups_judged(stories: &[StoryText], judge: Option<&Judge>) -> Vec<(i64, i64)> {
    let vectors = word_vectors(stories);
    let titles: Vec<HashSet<String>> = stories.iter().map(|s| title_words(&s.title)).collect();

    struct Group {
        id: i64,
        centroid: HashMap<String, f32>,
        latest: i64,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut group_index: HashMap<i64, usize> = HashMap::new();
    // Which groups contain each word (to compare only with groups that share one)
    let mut by_word: HashMap<String, HashSet<usize>> = HashMap::new();

    let add_to = |group: &mut Group, vector: &HashMap<String, f32>, time: i64| {
        for (word, weight) in vector {
            *group.centroid.entry(word.clone()).or_default() += weight;
        }
        group.latest = group.latest.max(time);
    };

    let mut order: Vec<usize> = (0..stories.len()).collect();
    order.sort_by_key(|&i| (stories[i].time, stories[i].id));

    // Stories placed so far, in time order, with their group's slot
    let mut placed: Vec<(usize, usize)> = Vec::new();
    let mut assigned = Vec::new();
    for i in order {
        let story = &stories[i];
        let vector = &vectors[i];
        let group_slot = match story.group {
            // Already grouped: just rebuild that group
            Some(group_id) => match group_index.get(&group_id) {
                Some(&slot) => slot,
                None => {
                    groups.push(Group { id: group_id, centroid: HashMap::new(), latest: story.time });
                    group_index.insert(group_id, groups.len() - 1);
                    groups.len() - 1
                }
            },
            None if judge.is_some_and(|j| j.embeddings.contains_key(&story.id)) => {
                let judge = judge.expect("checked");
                let pair = |j: usize| crate::pair_model::PairStory {
                    story: &stories[j],
                    embedding: &judge.embeddings[&stories[j].id],
                    words: &vectors[j],
                    title: &titles[j],
                };
                let this = pair(i);
                // Each open group's recent members' chances of being the
                // same news as this story (unlikely ones not worked out)
                let mut chances: HashMap<usize, (usize, Vec<f32>)> = HashMap::new();
                for &(j, slot) in placed.iter().rev() {
                    if story.time - stories[j].time > WINDOW_SECONDS {
                        break;
                    }
                    if !judge.embeddings.contains_key(&stories[j].id) {
                        continue;
                    }
                    let (members, found) = chances.entry(slot).or_default();
                    *members += 1;
                    let key = (story.id.min(stories[j].id), story.id.max(stories[j].id));
                    let chance = match judge.user_labels.get(&key) {
                        Some(&same) => same as u8 as f32,
                        None => judge.model.probability_if_close(&this, &pair(j)),
                    };
                    if chance > 0.0 {
                        found.push(chance);
                    }
                }
                let best = chances
                    .into_iter()
                    .filter(|(_, (_, found))| !found.is_empty())
                    .map(|(slot, (members, mut found))| {
                        found.sort_by(|a, b| b.total_cmp(a));
                        let k = LINKAGE.min(members);
                        (slot, found.iter().take(k).sum::<f32>() / k as f32)
                    })
                    .filter(|(_, chance)| *chance >= JOIN_CHANCE)
                    .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
                let slot = match best {
                    Some((slot, _)) => slot,
                    None => {
                        groups.push(Group { id: story.id, centroid: HashMap::new(), latest: story.time });
                        group_index.insert(story.id, groups.len() - 1);
                        groups.len() - 1
                    }
                };
                assigned.push((story.id, groups[slot].id));
                slot
            }
            None => {
                // The most similar open group sharing at least one word
                let candidates: HashSet<usize> = vector
                    .keys()
                    .filter_map(|word| by_word.get(word))
                    .flatten()
                    .copied()
                    .filter(|&slot| story.time - groups[slot].latest <= WINDOW_SECONDS)
                    .collect();
                let best = candidates
                    .into_iter()
                    .filter(|&slot| {
                        let centroid = &groups[slot].centroid;
                        vector.keys().filter(|word| centroid.contains_key(*word)).count() >= MIN_SHARED_WORDS
                    })
                    .map(|slot| (slot, cosine(vector, &groups[slot].centroid)))
                    .filter(|(_, similarity)| *similarity >= THRESHOLD)
                    .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
                let slot = match best {
                    Some((slot, _)) => slot,
                    None => {
                        groups.push(Group { id: story.id, centroid: HashMap::new(), latest: story.time });
                        group_index.insert(story.id, groups.len() - 1);
                        groups.len() - 1
                    }
                };
                assigned.push((story.id, groups[slot].id));
                slot
            }
        };
        add_to(&mut groups[group_slot], vector, story.time);
        placed.push((i, group_slot));
        for word in vector.keys() {
            by_word.entry(word.clone()).or_default().insert(group_slot);
        }
    }
    assigned
}

/// A feed's stories at least this many, before its boilerplate is worked out
const BOILERPLATE_MIN_STORIES: usize = 8;
/// A word in this share of a feed's stories is part of its template
const BOILERPLATE_SHARE: f32 = 0.15;

/// For each feed, the words that appear in a large share of its stories:
/// its template ("Promo Code: 20% Off | November 2026", a show's name in
/// every episode title). They'd otherwise make a feed's unrelated stories
/// look alike. Other feeds' use of the same words still counts.
fn feed_boilerplate(stories: &[StoryText], words: &[HashMap<String, f32>]) -> HashMap<i64, HashSet<String>> {
    let mut per_feed: HashMap<i64, (usize, HashMap<&str, usize>)> = HashMap::new();
    for (story, story_words) in stories.iter().zip(words) {
        let (count, word_counts) = per_feed.entry(story.feed).or_default();
        *count += 1;
        for word in story_words.keys() {
            *word_counts.entry(word.as_str()).or_default() += 1;
        }
    }
    per_feed
        .into_iter()
        .filter(|(_, (count, _))| *count >= BOILERPLATE_MIN_STORIES)
        .map(|(feed, (count, word_counts))| {
            let common = word_counts
                .into_iter()
                .filter(|(_, n)| *n as f32 / count as f32 >= BOILERPLATE_SHARE)
                .map(|(word, _)| word.to_string())
                .collect();
            (feed, common)
        })
        .collect()
}

/// A headline's significant words
pub fn title_words(title: &str) -> HashSet<String> {
    words(title).collect()
}

/// A story's significant words and how much each counts (title words twice)
fn story_words(story: &StoryText) -> HashMap<String, f32> {
    let mut counts: HashMap<String, f32> = HashMap::new();
    for word in words(&story.title) {
        *counts.entry(word).or_default() += 2.0;
    }
    let text: String = story.text.chars().take(TEXT_CHARS).collect();
    for word in words(&text) {
        *counts.entry(word).or_default() += 1.0;
    }
    counts
}

/// The significant words of `text`, lowercased and lightly normalized
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split_whitespace()
        // Web addresses (and Markdown links to them) say nothing about the story
        .filter(|token| !token.contains("://") && !token.starts_with("www.") && !token.contains("]("))
        .flat_map(|token| token.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’')))
        .map(|word| word.trim_matches(|c| c == '\'' || c == '’').to_lowercase())
        .filter(|word| !word.is_empty())
        // Link markers ("[1]") and stray numbers say nothing about the story
        .filter(|word| !word.chars().all(|c| c.is_ascii_digit()) || word.len() == 4)
        .map(|word| normalize_word(&word))
        .filter(|word| word.chars().count() >= 3 && !STOPWORDS.contains(&word.as_str()))
}

/// Folds possessives and plurals together ("Musk's" = "musk", "cars" = "car")
fn normalize_word(word: &str) -> String {
    let word = word.trim_end_matches("'s").trim_end_matches("’s");
    if word.len() > 4 && word.ends_with("ies") {
        format!("{}y", &word[..word.len() - 3])
    } else if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") {
        word[..word.len() - 1].to_string()
    } else {
        word.to_string()
    }
}

fn normalize(vector: &mut HashMap<String, f32>) {
    let length = vector.values().map(|w| w * w).sum::<f32>().sqrt();
    if length > 0.0 {
        for weight in vector.values_mut() {
            *weight /= length;
        }
    }
}

pub fn cosine(a: &HashMap<String, f32>, b: &HashMap<String, f32>) -> f32 {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let dot: f32 = small.iter().filter_map(|(word, weight)| large.get(word).map(|other| weight * other)).sum();
    let b_length = b.values().map(|w| w * w).sum::<f32>().sqrt();
    let a_length = a.values().map(|w| w * w).sum::<f32>().sqrt();
    if a_length == 0.0 || b_length == 0.0 { 0.0 } else { dot / (a_length * b_length) }
}

/// Common English words, which say nothing about what a story is about
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "with", "this", "from", "are", "was", "were", "has", "have", "had", "but", "not",
    "you", "your", "they", "their", "them", "his", "her", "she", "him", "its", "our", "who", "what", "when",
    "where", "why", "how", "which", "will", "would", "can", "could", "should", "may", "might", "been", "being",
    "into", "about", "after", "before", "over", "under", "than", "then", "there", "here", "out", "all", "any",
    "more", "most", "some", "such", "only", "also", "just", "now", "new", "one", "two", "first", "last", "says",
    "said", "say", "get", "gets", "got", "make", "makes", "made", "like", "year", "years", "time", "day", "days",
    "week", "way", "back", "still", "even", "much", "many", "very", "own", "off", "too", "per", "via", "these",
    "those", "because", "while", "during", "between", "against", "without", "within", "through", "since",
    "until", "again", "other", "another", "each", "every", "both", "few", "less", "least", "same", "around",
    "does", "did", "doing", "done", "going", "goe", "see", "know", "take", "want", "need", "use", "used",
    "next", "people", "report", "reports", "news", "read", "here’s", "here's", "it’s", "it's", "don’t", "don't",
    "can’t", "can't", "won’t", "won't", "isn’t", "isn't", "what’s", "what's", "that’s", "that's", "post",
    "appeared", "article", "continue", "reading", "click", "comments", "comment", "share", "subscribe", "update",
    "opinion", "video", "watch", "live", "today", "yesterday", "tomorrow", "monday", "tuesday", "wednesday",
    "thursday", "friday", "saturday", "sunday", "via", "amp", "nbsp", "image", "photo", "credit", "getty",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn story(id: i64, hours: i64, title: &str, text: &str) -> StoryText {
        StoryText { id, feed: id, time: hours * 3600, title: title.into(), text: text.into(), group: None }
    }

    #[test]
    fn same_event_groups_and_others_dont() {
        let stories = vec![
            story(1, 0, "Spirit Airlines data sold to Google in bankruptcy sale", "A judge approved the sale of Spirit Airlines data."),
            story(2, 2, "Judge approves Google purchase of Spirit Airlines business data", "Google will pay $10 million for Spirit data."),
            story(3, 3, "GTA 6 reviews: Rockstar's Vice City is a breathable world", "Grand Theft Auto VI arrives."),
            story(4, 5, "Rockstar GTA 6 review roundup", "Critics praise Grand Theft Auto VI and Vice City."),
            story(5, 6, "Local bakery wins award for sourdough", "The bakery on Main Street won."),
            // The same airline story five days on: its group has gone quiet
            story(6, 24 * 5, "Spirit Airlines data sale to Google completed", "Spirit Airlines data now with Google."),
        ];
        let groups: HashMap<i64, i64> = assign_groups(&stories).into_iter().collect();
        assert_eq!(groups[&2], groups[&1], "the two Spirit/Google stories");
        assert_eq!(groups[&4], groups[&3], "the two GTA 6 stories");
        assert_ne!(groups[&3], groups[&1]);
        assert_eq!(groups[&5], 5, "unrelated: its own group");
        assert_eq!(groups[&6], 6, "too late to join");
    }

    #[test]
    fn new_stories_join_existing_groups() {
        let mut stories = vec![
            story(1, 0, "Spirit Airlines data sold to Google in bankruptcy sale", "A judge approved the sale."),
            story(2, 1, "Judge approves Google purchase of Spirit Airlines data", "Google pays for Spirit data."),
        ];
        stories[0].group = Some(1);
        stories[1].group = Some(1);
        stories.push(story(3, 4, "Spirit Airlines' data now belongs to Google", "The airline data sale closed."));
        assert_eq!(assign_groups(&stories), [(3, 1)]);
    }

    /// A feed's template words don't make its stories one group
    #[test]
    fn feed_templates_dont_group() {
        let brands = ["Instacart", "Peacock", "Dell", "Chatbooks", "Gametime", "Western Digital", "Lenovo", "Nike", "Adidas", "Samsung"];
        let mut stories: Vec<StoryText> = brands
            .iter()
            .enumerate()
            .map(|(i, brand)| StoryText {
                id: i as i64 + 1,
                feed: 1,
                time: i as i64 * 3600,
                title: format!("{} Promo Code: 20% Off | November 2026", brand),
                text: String::new(),
                group: None,
            })
            .collect();
        // Another feed's similar promo story isn't affected by feed 1's template
        stories.push(StoryText { id: 99, feed: 2, time: 20 * 3600, title: "Nike promo code: 20% off".into(), text: String::new(), group: None });
        let groups: HashMap<i64, i64> = assign_groups(&stories).into_iter().collect();
        for id in 1..=10 {
            assert_eq!(groups[&id], id, "story {} grouped", id);
        }
    }

    /// Folders are grouped only when chosen, each with only its own
    /// stories; moving a feed, turning grouping off and deleting a folder
    /// keep groups within grouped folders.
    #[test]
    fn grouping_stays_within_folders() {
        use crate::db::Database;
        use crate::models::ArticleFilter;
        let db = Database::open_in_memory().unwrap();
        let news = db.test_grouping_folder();
        let tech = db.create_folder("Tech").unwrap();
        db.set_folder_grouping(tech, true).unwrap();
        let other = db.create_folder("Other").unwrap();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, Some(news)).unwrap();
        let ap = db.add_or_update_feed("https://ap.example/rss", "AP", None, None, Some(news)).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, Some(tech)).unwrap();
        let blog = db.add_or_update_feed("https://blog.example/rss", "Blog", None, None, Some(other)).unwrap();
        let base = chrono::Utc::now() - chrono::Duration::hours(10);
        let add = |feed: i64, hash: &str, hours: i64, title: &str| {
            db.upsert_article(feed, None, hash, title, None, None, Some(title), None, Some(title),
                Some(base + chrono::Duration::hours(hours)), None).unwrap();
        };
        // The same news in every folder
        add(reuters, "a", 0, "Spirit Airlines data sold to Google in bankruptcy sale");
        add(ap, "b", 1, "Google buys Spirit Airlines data in bankruptcy deal");
        add(verge, "c", 2, "Spirit Airlines bankruptcy sale sends data to Google");
        add(blog, "d", 3, "Google gets Spirit Airlines data in bankruptcy sale");
        group_new_stories(&db).unwrap();
        let group_of = |hash: &str| {
            let all = db.get_articles(&ArticleFilter::default()).unwrap();
            let titles = ["a", "b", "c", "d"];
            let n = titles.iter().position(|t| *t == hash).unwrap();
            let mut by_date = all.clone();
            by_date.sort_by_key(|a| a.shown_date());
            by_date[n].group_id
        };
        assert_eq!(group_of("a"), group_of("b"), "together in News");
        assert!(group_of("c").is_some() && group_of("c") != group_of("a"), "Tech's on its own");
        assert_eq!(group_of("d"), None, "Other isn't grouped");

        // The Verge moves to News: it leaves Tech, and joins News's group
        db.set_feed_folder(verge, Some(news)).unwrap();
        group_new_stories(&db).unwrap();
        assert_eq!(group_of("c"), group_of("a"));

        // AP moves to Other: its story leaves the group, the rest stay together
        db.set_feed_folder(ap, Some(other)).unwrap();
        group_new_stories(&db).unwrap();
        assert_eq!(group_of("b"), None);
        assert_eq!(group_of("a"), group_of("c"));

        // Reuters, whose story names the group, moves to Tech: it's grouped
        // there, and the Verge's story (alone in News) gets a group of its own
        db.set_feed_folder(reuters, Some(tech)).unwrap();
        group_new_stories(&db).unwrap();
        assert_ne!(group_of("a"), group_of("c"));
        assert!(group_of("a").is_some() && group_of("c").is_some());

        // Turning News off, or deleting Tech, ungroups their stories
        db.set_folder_grouping(news, false).unwrap();
        db.delete_folder(tech).unwrap();
        group_new_stories(&db).unwrap();
        assert_eq!((group_of("a"), group_of("c")), (None, None));
    }

    /// What a folder's grouping learned: found, and deleted to start fresh,
    /// without touching other folders'.
    #[test]
    fn a_folders_grouping_data() {
        use crate::db::Database;
        use crate::models::ArticleFilter;
        let db = Database::open_in_memory().unwrap();
        let news = db.test_grouping_folder();
        let tech = db.create_folder("Tech").unwrap();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, Some(news)).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, Some(tech)).unwrap();
        for (feed, hash) in [(reuters, "a"), (reuters, "b"), (verge, "c"), (verge, "d")] {
            db.upsert_article(feed, None, hash, hash, None, None, None, None, None, None, None).unwrap();
        }
        let id = |hash: &str| db.get_articles(&ArticleFilter::default()).unwrap().into_iter().find(|a| a.title == hash).unwrap().id;
        let (a, b, c, d) = (id("a"), id("b"), id("c"), id("d"));
        assert!(!db.has_grouping_data(news).unwrap());
        db.set_pair_labels(&[((a, b), Some(true)), ((c, d), Some(false)), ((a, c), Some(false))], "user").unwrap();
        db.set_setting(&model_setting(tech), "{}").unwrap();
        assert!(db.has_grouping_data(news).unwrap(), "a pair of its stories");
        assert!(db.has_grouping_data(tech).unwrap());

        db.clear_grouping_data(news).unwrap();
        assert!(!db.has_grouping_data(news).unwrap());
        let left: Vec<(i64, i64)> = db.pair_labels().unwrap().into_iter().map(|l| (l.0, l.1)).collect();
        assert_eq!(left.len(), 2, "Tech's pair, and the one across folders, stay: {:?}", left);
        assert!(db.has_grouping_data(tech).unwrap());

        // Deleting a folder deletes its model
        db.delete_folder(tech).unwrap();
        assert_eq!(db.get_setting(&model_setting(tech)).unwrap(), None);
    }

    /// Grouping through the database: stories get groups, a later story
    /// joins its group, and a group's members can be fetched for a view.
    #[test]
    fn groups_through_the_database() {
        use crate::db::Database;
        use crate::models::ArticleFilter;
        let db = Database::open_in_memory().unwrap();
        let news = db.test_grouping_folder();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, Some(news)).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, Some(news)).unwrap();
        let base = chrono::Utc::now() - chrono::Duration::hours(10);
        let add = |feed: i64, hash: &str, hours: i64, title: &str, text: &str| {
            db.upsert_article(feed, None, hash, title, None, None, Some(text), None, Some(text),
                Some(base + chrono::Duration::hours(hours)), None).unwrap();
        };
        add(reuters, "a", 0, "Spirit Airlines data sold to Google in bankruptcy sale", "A judge approved the sale of Spirit Airlines data to Google.");
        add(verge, "b", 1, "Google buys Spirit Airlines data for $10 million", "Spirit Airlines business data goes to Google.");
        add(verge, "c", 2, "Apple releases a new iPad", "The tablet has a faster chip.");
        assert_eq!(group_new_stories(&db).unwrap(), 3);

        let all = db.get_articles(&ArticleFilter::default()).unwrap();
        let group_of = |title: &str| all.iter().find(|a| a.title.starts_with(title)).unwrap().group();
        assert_eq!(group_of("Spirit"), group_of("Google buys"));
        assert_ne!(group_of("Spirit"), group_of("Apple"));

        // A refresh brings a follow-up: it joins the existing group
        add(reuters, "d", 5, "Spirit Airlines' data sale to Google is approved", "Google gets Spirit Airlines data.");
        assert_eq!(group_new_stories(&db).unwrap(), 1);
        let spirit = group_of("Spirit");
        let members = db.group_members(&ArticleFilter::default(), &[spirit]).unwrap();
        assert_eq!(members.len(), 3);
        // Within a view: only that feed's members
        let verge_only = db.group_members(&ArticleFilter { feed_id: Some(verge), ..Default::default() }, &[spirit]).unwrap();
        assert_eq!(verge_only.len(), 1);
        // Nothing left to group
        assert_eq!(group_new_stories(&db).unwrap(), 0);
    }

    /// One word in common isn't enough
    #[test]
    fn one_shared_word_doesnt_group() {
        let stories = vec![
            story(1, 0, "007 First Light review", "A spy game."),
            story(2, 1, "Bike Light, white LED strip", "For cycling at night."),
            story(3, 2, "Light-Day", "A comic."),
        ];
        let groups: HashMap<i64, i64> = assign_groups(&stories).into_iter().collect();
        assert_eq!((groups[&1], groups[&2], groups[&3]), (1, 2, 3));
    }

    /// With the learned judge: alike stories group, unlike ones don't, and
    /// the user's own answer about a pair stands whatever the model thinks
    #[test]
    fn judged_grouping_follows_the_user() {
        use crate::pair_model::{Example, PairModel};
        let mut examples = Vec::new();
        for i in 0..20 {
            let j = i as f32 * 0.01;
            examples.push(Example { features: [0.9 - j, 0.6, 0.5, 0.2, 0.0], same: true, weight: 1.0 });
            examples.push(Example { features: [0.5 + j, 0.05, 0.0, 0.5, 0.0], same: false, weight: 1.0 });
        }
        let model = PairModel::train(&examples).unwrap();
        let unit = |v: [f32; 3]| {
            let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / l).collect::<Vec<f32>>()
        };
        let stories = vec![
            story(1, 0, "Spirit Airlines data sold to Google in bankruptcy sale", "A judge approved the sale."),
            story(2, 1, "Judge approves Google purchase of Spirit Airlines data", "Google pays for Spirit data."),
            story(3, 2, "Local bakery wins award for sourdough", "The bakery on Main Street won."),
        ];
        let embeddings: HashMap<i64, Vec<f32>> =
            [(1, unit([1.0, 0.1, 0.0])), (2, unit([1.0, 0.12, 0.0])), (3, unit([0.0, 0.2, 1.0]))].into();
        let mut user_labels = HashMap::new();
        let judge = Judge { model: &model, embeddings: &embeddings, user_labels: &user_labels };
        let groups: HashMap<i64, i64> = assign_groups_judged(&stories, Some(&judge)).into_iter().collect();
        assert_eq!((groups[&1], groups[&2], groups[&3]), (1, 1, 3));
        // The user says 1 and 2 are different stories
        user_labels.insert((1, 2), false);
        let judge = Judge { model: &model, embeddings: &embeddings, user_labels: &user_labels };
        let groups: HashMap<i64, i64> = assign_groups_judged(&stories, Some(&judge)).into_iter().collect();
        assert_eq!((groups[&1], groups[&2]), (1, 2));
    }

    #[test]
    fn words_are_normalized() {
        let found: Vec<String> = words("Musk's cars: the [1] https://x.com stories of 2026, 42 companies").collect();
        assert_eq!(found, ["musk", "car", "story", "2026", "company"]);
    }

    /// Groups every story in an archive and prints the groups, to judge and
    /// tune the matching: `SHREADER_DB=/path/to/copy.db cargo test --release
    /// print_groups -- --ignored --nocapture` (optionally `GROUP_THRESHOLD=0.3`)
    #[test]
    #[ignore]
    fn print_groups() {
        let path = std::env::var("SHREADER_DB").expect("set SHREADER_DB");
        let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT a.id, MIN(COALESCE(a.published_at, a.fetched_at), a.fetched_at), a.title,
                        substr(COALESCE(a.content_text, ''), 1, 600), f.title, a.feed_id
                 FROM articles a JOIN feeds f ON f.id = a.feed_id",
            )
            .unwrap();
        let mut feeds = HashMap::new();
        let stories: Vec<StoryText> = stmt
            .query_map([], |r| {
                let date: String = r.get(1)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    date,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .map(|(id, date, title, text, feed, feed_id)| {
                feeds.insert(id, feed);
                let time = chrono::DateTime::parse_from_rfc3339(&date).map(|d| d.timestamp()).unwrap_or(0);
                StoryText { id, feed: feed_id, time, title, text, group: None }
            })
            .collect();
        let start = std::time::Instant::now();
        let assigned = assign_groups(&stories);
        let elapsed = start.elapsed();
        let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
        for (story, group) in &assigned {
            members.entry(*group).or_default().push(*story);
        }
        let titles: HashMap<i64, &str> = stories.iter().map(|s| (s.id, s.title.as_str())).collect();
        let mut groups: Vec<&Vec<i64>> = members.values().filter(|m| m.len() > 1).collect();
        groups.sort_by_key(|m| std::cmp::Reverse(m.len()));
        let grouped: usize = groups.iter().map(|m| m.len()).sum();
        eprintln!(
            "{} stories -> {} groups of 2+ holding {} stories; largest {}; took {:?}",
            stories.len(),
            groups.len(),
            grouped,
            groups.first().map_or(0, |m| m.len()),
            elapsed
        );
        let show: usize = std::env::var("SHOW").ok().and_then(|n| n.parse().ok()).unwrap_or(40);
        for group in groups.iter().take(show) {
            eprintln!("--- {} stories", group.len());
            let limit: usize = std::env::var("MEMBERS").ok().and_then(|n| n.parse().ok()).unwrap_or(6);
            for id in group.iter().take(limit) {
                eprintln!("    [{}] {}", feeds[id], titles[id]);
            }
            if group.len() > limit {
                eprintln!("    … and {} more", group.len() - limit);
            }
        }
    }
}
