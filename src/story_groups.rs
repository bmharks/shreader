//! Story grouping: stories about the same thing, from any feed, become one
//! group (the same event from several sources, or a site's follow-ups on a
//! developing story).
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

/// Groups every story not grouped yet, oldest first, in batches. Returns how
/// many were grouped. Run after refreshes, off the UI thread.
pub fn group_new_stories(db: &crate::db::Database) -> anyhow::Result<usize> {
    let mut grouped = 0;
    loop {
        let stories = db.stories_to_group(BATCH, WINDOW_SECONDS)?;
        let assigned = assign_groups(&stories);
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

    // Each story as a normalized weighted-word vector
    let vectors: Vec<HashMap<String, f32>> = words
        .iter()
        .map(|story_words| {
            let mut vector: HashMap<String, f32> =
                story_words.iter().map(|(word, count)| (word.clone(), count * idf(word))).collect();
            normalize(&mut vector);
            vector
        })
        .collect();

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

fn cosine(a: &HashMap<String, f32>, b: &HashMap<String, f32>) -> f32 {
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

    /// Grouping through the database: stories get groups, a later story
    /// joins its group, and a group's members can be fetched for a view.
    #[test]
    fn groups_through_the_database() {
        use crate::db::Database;
        use crate::models::ArticleFilter;
        let db = Database::open_in_memory().unwrap();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, None).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, None).unwrap();
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
