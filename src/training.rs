//! Training examples for story grouping: pairs of stories labelled as the
//! same news or not.
//!
//! To start, a local Ollama model labels recent pairs (`label_with_ollama`);
//! the user's corrections are added as labels of their own, which count for
//! more and override Ollama's. A light model learns from them (see
//! `pair_model`).
//!
//! Everything is per folder, as grouping is: a folder's pairs are two of its
//! own stories, and its model learns only from them (a pair whose stories
//! are no longer in one folder, after a feed moved, is kept but unused).
//!
//! Only promising pairs are worth asking about: for each recent story, its
//! few nearest neighbours by embedding, from around the same time. Pairs
//! that far apart in meaning are different stories anyway.

use std::collections::{HashMap, HashSet};

use futures::stream::{self, StreamExt};

use crate::db::Database;
use crate::ollama::Ollama;

/// Labels from Ollama
pub const SOURCE_OLLAMA: &str = "ollama";
/// Labels from the user (they win over Ollama's)
pub const SOURCE_USER: &str = "user";

/// How far back the initial labelling goes
pub const LABEL_DAYS: i64 = 7;

/// Each story's nearest neighbours asked about
const NEIGHBOURS: usize = 3;

/// Pairs less alike than this (embedding similarity) aren't asked about
const MIN_SIMILARITY: f32 = 0.45;

/// Pairs further apart in time than this aren't asked about (as for grouping)
const WINDOW_SECONDS: i64 = crate::story_groups::WINDOW_SECONDS;

/// Questions in flight at once (keeps the GPU busy without crowding it)
const CONCURRENT_QUESTIONS: usize = 3;

/// Gives up after this many errors in a row (Ollama stopped, model removed…)
const MAX_ERRORS_IN_A_ROW: usize = 5;

/// The pairs worth labelling among `stories` (id, Unix time, embedding):
/// each story with its `NEIGHBOURS` most similar others within the time
/// window, if similar enough. Smaller id first, no repeats.
pub fn candidate_pairs(stories: &[(i64, i64, Vec<f32>)]) -> Vec<(i64, i64)> {
    let mut order: Vec<usize> = (0..stories.len()).collect();
    order.sort_by_key(|&i| stories[i].1);
    let mut pairs = HashSet::new();
    for (position, &i) in order.iter().enumerate() {
        let (id, time, embedding) = &stories[i];
        // Only stories within the window: walk outwards from this one in
        // time order
        let mut nearest: Vec<(f32, i64)> = Vec::new();
        let earlier = order[..position].iter().rev().take_while(|&&j| time - stories[j].1 <= WINDOW_SECONDS);
        let later = order[position + 1..].iter().take_while(|&&j| stories[j].1 - time <= WINDOW_SECONDS);
        for &j in earlier.chain(later) {
            let similarity = crate::embeddings::similarity(embedding, &stories[j].2);
            if similarity >= MIN_SIMILARITY {
                nearest.push((similarity, stories[j].0));
            }
        }
        nearest.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, other) in nearest.into_iter().take(NEIGHBOURS) {
            pairs.insert((*id.min(&other), *id.max(&other)));
        }
    }
    let mut pairs: Vec<(i64, i64)> = pairs.into_iter().collect();
    pairs.sort_unstable();
    pairs
}

/// A folder's recent pairs Ollama hasn't labelled yet, with their titles.
fn pairs_to_label(db: &Database, folder: i64) -> anyhow::Result<Vec<(i64, i64, String, String)>> {
    let since = chrono::Utc::now() - chrono::Duration::days(LABEL_DAYS);
    let stories = db.recent_embedded_stories(&since.to_rfc3339(), folder)?;
    let done = db.labelled_pairs(SOURCE_OLLAMA)?;
    let pairs: Vec<(i64, i64)> = candidate_pairs(&stories).into_iter().filter(|p| !done.contains(p)).collect();
    let ids: Vec<i64> = pairs.iter().flat_map(|&(a, b)| [a, b]).collect::<HashSet<_>>().into_iter().collect();
    let titles = db.titles(&ids)?;
    Ok(pairs
        .into_iter()
        .filter_map(|(a, b)| Some((a, b, titles.get(&a)?.clone(), titles.get(&b)?.clone())))
        .collect())
}

/// How a labelling run is going
#[derive(Debug, Clone)]
pub enum LabelEvent {
    /// Recent stories are being analysed (embeddings) first
    Preparing,
    /// `done` of `total` pairs labelled, `same` of them the same story
    Progress { done: usize, total: usize, same: usize },
    Finished(Result<usize, String>),
}

/// Labels folder `folder`'s recent pairs Ollama hasn't labelled yet, with
/// `model`, reporting through `report`. Resumes where an earlier run
/// stopped. `prepare` first makes sure recent stories have embeddings.
pub async fn label_with_ollama<P, Fut>(
    db: Database,
    ollama: Ollama,
    model: String,
    folder: i64,
    prepare: P,
    mut report: futures::channel::mpsc::Sender<LabelEvent>,
) where
    P: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    use futures::SinkExt;
    let _ = report.send(LabelEvent::Preparing).await;
    if let Err(e) = prepare().await {
        let _ = report.send(LabelEvent::Finished(Err(e))).await;
        return;
    }
    let pairs = {
        let db = db.clone();
        match tokio::task::spawn_blocking(move || pairs_to_label(&db, folder)).await {
            Ok(Ok(pairs)) => pairs,
            Ok(Err(e)) => {
                let _ = report.send(LabelEvent::Finished(Err(format!("{:#}", e)))).await;
                return;
            }
            Err(e) => {
                let _ = report.send(LabelEvent::Finished(Err(e.to_string()))).await;
                return;
            }
        }
    };
    let total = pairs.len();
    let _ = report.send(LabelEvent::Progress { done: 0, total, same: 0 }).await;

    let mut answers = stream::iter(pairs)
        .map(|(a, b, title_a, title_b)| {
            let ollama = ollama.clone();
            let model = model.clone();
            async move { (a, b, ollama.same_story(&model, &title_a, &title_b).await) }
        })
        .buffer_unordered(CONCURRENT_QUESTIONS);
    let (mut done, mut same, mut errors_in_a_row) = (0, 0, 0);
    while let Some((a, b, answer)) = answers.next().await {
        match answer {
            Ok(is_same) => {
                errors_in_a_row = 0;
                if let Err(e) = db.set_pair_label(a, b, SOURCE_OLLAMA, is_same) {
                    let _ = report.send(LabelEvent::Finished(Err(format!("{:#}", e)))).await;
                    return;
                }
                done += 1;
                same += is_same as usize;
                let _ = report.send(LabelEvent::Progress { done, total, same }).await;
            }
            Err(e) => {
                errors_in_a_row += 1;
                tracing::warn!("Ollama couldn't label a pair: {}", e);
                if errors_in_a_row >= MAX_ERRORS_IN_A_ROW {
                    let _ = report.send(LabelEvent::Finished(Err(e))).await;
                    return;
                }
            }
        }
    }
    let _ = report.send(LabelEvent::Finished(Ok(done))).await;
}

/// Labelled pairs needed before the learned model is used for grouping
pub const MIN_EXAMPLES: usize = 200;

/// How much one of the user's answers counts, against one of Ollama's
const USER_WEIGHT: f32 = 5.0;

/// Every labelled pair of a folder's stories as a training example (pairs
/// whose stories are gone, or have no embedding, left out). The user's
/// answer about a pair replaces Ollama's. Returns ((a, b), example).
pub fn labelled_examples(db: &Database, folder: i64) -> anyhow::Result<Vec<((i64, i64), crate::pair_model::Example)>> {
    let mut labels: HashMap<(i64, i64), (bool, f32)> = HashMap::new();
    for (a, b, source, same) in db.pair_labels_in(folder)? {
        let weight = if source == SOURCE_USER { USER_WEIGHT } else { 1.0 };
        let entry = labels.entry((a, b)).or_insert((same, weight));
        if source == SOURCE_USER {
            *entry = (same, weight);
        }
    }
    let ids: Vec<i64> = labels.keys().flat_map(|&(a, b)| [a, b]).collect::<HashSet<_>>().into_iter().collect();
    // Words are weighed against the folder's recent stories, as when grouping
    let since = chrono::Utc::now() - chrono::Duration::days(crate::embeddings::RECENT_DAYS);
    let stories = db.story_texts(&since.to_rfc3339(), folder, &ids)?;
    let embeddings = db.get_embeddings(&ids)?;
    let vectors = crate::story_groups::word_vectors(&stories);
    let titles: Vec<HashSet<String>> = stories.iter().map(|s| crate::story_groups::title_words(&s.title)).collect();
    let index: HashMap<i64, usize> = stories.iter().enumerate().map(|(i, s)| (s.id, i)).collect();
    let pair_story = |id: i64| -> Option<crate::pair_model::PairStory> {
        let i = *index.get(&id)?;
        Some(crate::pair_model::PairStory {
            story: &stories[i],
            embedding: embeddings.get(&id)?,
            words: &vectors[i],
            title: &titles[i],
        })
    };
    let mut examples: Vec<((i64, i64), crate::pair_model::Example)> = labels
        .into_iter()
        .filter_map(|((a, b), (same, weight))| {
            let features = crate::pair_model::features(&pair_story(a)?, &pair_story(b)?);
            Some(((a, b), crate::pair_model::Example { features, same, weight }))
        })
        .collect();
    examples.sort_by_key(|(pair, _)| *pair);
    Ok(examples)
}

/// Whether a pair is one of the fifth held back to test the model on
/// (the same pairs every time)
pub fn held_out(pair: (i64, i64)) -> bool {
    (pair.0.wrapping_mul(31).wrapping_add(pair.1)).rem_euclid(5) == 0
}

/// A trained model, with how it did on examples held back from it
#[derive(Debug, Clone)]
pub struct Trained {
    pub model: crate::pair_model::PairModel,
    pub examples: usize,
    pub score: crate::pair_model::Score,
}

/// Trains a folder's grouping model on its labels. `None` if there aren't
/// enough yet. Tests it on a fifth of them first (trained without those),
/// then learns from them all. CPU work: off the UI thread.
pub fn train(db: &Database, folder: i64) -> anyhow::Result<Option<Trained>> {
    use crate::pair_model::{Example, PairModel, Score};
    let examples = labelled_examples(db, folder)?;
    if examples.len() < MIN_EXAMPLES {
        return Ok(None);
    }
    let (test, learn): (Vec<_>, Vec<_>) = examples.iter().partition(|(pair, _)| held_out(*pair));
    let learn: Vec<Example> = learn.into_iter().map(|(_, e)| e.clone()).collect();
    let score = match PairModel::train(&learn) {
        Some(model) => Score::of(test.iter().map(|(_, e)| (model.probability(&e.features) >= 0.5, e.same))),
        None => Score::default(),
    };
    let all: Vec<Example> = examples.into_iter().map(|(_, e)| e).collect();
    Ok(PairModel::train(&all).map(|model| Trained { model, examples: all.len(), score }))
}

/// What Settings says about a folder's trained model
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub examples: usize,
    pub tested: usize,
    pub accuracy: f32,
}

impl ModelInfo {
    fn to_json(&self) -> String {
        serde_json::json!({ "examples": self.examples, "tested": self.tested, "accuracy": self.accuracy }).to_string()
    }

    /// A folder's, if it has a model
    pub fn load(db: &Database, folder: i64) -> Option<Self> {
        let key = crate::story_groups::model_info_setting(folder);
        let value: serde_json::Value = serde_json::from_str(&db.get_setting(&key).ok()??).ok()?;
        Some(Self {
            examples: value["examples"].as_u64()? as usize,
            tested: value["tested"].as_u64()? as usize,
            accuracy: value["accuracy"].as_f64()? as f32,
        })
    }
}

/// Whether a folder has a model grouping can use
pub fn has_model(db: &Database, folder: i64) -> bool {
    db.get_setting(&crate::story_groups::model_setting(folder))
        .ok()
        .flatten()
        .and_then(|json| crate::pair_model::PairModel::from_json(&json))
        .is_some()
}

/// Trains a folder's grouping model on its labels and, if there are
/// enough, stores it and groups the folder's recent stories again with it.
/// CPU work: off the UI thread.
pub fn train_and_regroup(db: &Database, folder: i64) -> anyhow::Result<Option<ModelInfo>> {
    let Some(trained) = train(db, folder)? else {
        return Ok(None);
    };
    let info = ModelInfo { examples: trained.examples, tested: trained.score.examples, accuracy: trained.score.accuracy() };
    db.set_setting(&crate::story_groups::model_setting(folder), &trained.model.to_json())?;
    db.set_setting(&crate::story_groups::model_info_setting(folder), &info.to_json())?;
    crate::story_groups::regroup_recent(db, folder)?;
    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let length = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / length).collect()
    }

    /// A folder's model learns only from pairs of its own stories, and is
    /// stored as its own: other folders' models aren't touched.
    #[test]
    fn each_folder_trains_on_its_own_pairs() {
        use crate::models::ArticleFilter;
        let db = Database::open_in_memory().unwrap();
        let news = db.test_grouping_folder();
        let tech = db.create_folder("Tech").unwrap();
        db.set_folder_grouping(tech, true).unwrap();
        let reuters = db.add_or_update_feed("https://reuters.example/rss", "Reuters", None, None, Some(news)).unwrap();
        let verge = db.add_or_update_feed("https://verge.example/rss", "The Verge", None, None, Some(tech)).unwrap();
        let base = chrono::Utc::now() - chrono::Duration::hours(30);
        // 30 News stories in 6 topics, and 4 Tech stories
        for n in 0..34 {
            let (feed, topic) = if n < 30 { (reuters, n % 6) } else { (verge, n % 2) };
            let title = format!("Topic{} story {}", topic, n);
            db.upsert_article(feed, None, &format!("h{}", n), &title, None, None, Some(&title), None, Some(&title),
                Some(base + chrono::Duration::minutes(n)), None).unwrap();
        }
        let stories = db.get_articles(&ArticleFilter::default()).unwrap();
        let topic = |a: &crate::models::Article| a.title[5..6].parse::<usize>().unwrap();
        let embedding = |t: usize| {
            let mut v = vec![0.02f32; crate::embeddings::DIMENSIONS];
            v[t] = 1.0;
            let length = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / length).collect::<Vec<f32>>()
        };
        let rows: Vec<(i64, Vec<u8>)> =
            stories.iter().map(|a| (a.id, crate::embeddings::to_bytes(&embedding(topic(a))))).collect();
        db.set_embeddings(&rows).unwrap();
        // Every pair labelled (same topic: the same story), across folders too
        let mut labels = Vec::new();
        for (i, a) in stories.iter().enumerate() {
            for b in &stories[i + 1..] {
                labels.push(((a.id, b.id), Some(topic(a) == topic(b))));
            }
        }
        db.set_pair_labels(&labels, SOURCE_OLLAMA).unwrap();

        let news_pairs = 30 * 29 / 2;
        assert_eq!(labelled_examples(&db, news).unwrap().len(), news_pairs, "only News's own pairs");
        assert_eq!(labelled_examples(&db, tech).unwrap().len(), 6);

        let info = train_and_regroup(&db, news).unwrap().expect("enough to learn from");
        assert_eq!(info.examples, news_pairs);
        assert!(has_model(&db, news));
        assert_eq!(ModelInfo::load(&db, news), Some(info));
        assert!(!has_model(&db, tech), "Tech's untouched");
        assert!(train_and_regroup(&db, tech).unwrap().is_none(), "too few to learn from");
        assert!(!has_model(&db, tech));
    }

    #[test]
    fn candidates_are_close_neighbours_in_time() {
        let day = 24 * 60 * 60;
        let stories = vec![
            (1, 0, unit(&[1.0, 0.0, 0.0])),
            (2, 3600, unit(&[0.9, 0.1, 0.0])),        // close to 1: a candidate
            (3, 7200, unit(&[0.0, 1.0, 0.0])),        // unlike anything
            (4, 10 * day, unit(&[1.0, 0.05, 0.0])),   // like 1, but much later
            (5, 10 * day + 60, unit(&[0.95, 0.0, 0.1])), // like 4: a candidate
        ];
        assert_eq!(candidate_pairs(&stories), [(1, 2), (4, 5)]);
    }

    #[test]
    fn each_story_asks_about_its_nearest_few() {
        // Ten near-identical stories: each brings its 3 nearest, not all 9
        let stories: Vec<(i64, i64, Vec<f32>)> =
            (0..10).map(|i| (i, i * 60, unit(&[1.0, i as f32 * 0.01, 0.0]))).collect();
        let pairs = candidate_pairs(&stories);
        assert!(pairs.len() < 45, "{} pairs", pairs.len());
        assert!(pairs.len() >= 10);
        assert!(pairs.iter().all(|(a, b)| a < b));
    }
}

