//! Training examples for story grouping: pairs of stories labelled as the
//! same news or not.
//!
//! To start, a local Ollama model labels recent pairs (`label_with_ollama`);
//! the user's corrections are added as labels of their own, which count for
//! more and override Ollama's. A light model learns from them (next stage).
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

/// The recent pairs Ollama hasn't labelled yet, with their titles.
fn pairs_to_label(db: &Database) -> anyhow::Result<Vec<(i64, i64, String, String)>> {
    let since = chrono::Utc::now() - chrono::Duration::days(LABEL_DAYS);
    let stories = db.recent_embedded_stories(&since.to_rfc3339())?;
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

/// Labels the recent pairs Ollama hasn't labelled yet, with `model`,
/// reporting through `report`. Resumes where an earlier run stopped.
/// `prepare` first makes sure recent stories have embeddings.
pub async fn label_with_ollama<P, Fut>(
    db: Database,
    ollama: Ollama,
    model: String,
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
        match tokio::task::spawn_blocking(move || pairs_to_label(&db)).await {
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

/// Every labelled pair as a training example (pairs whose stories are gone,
/// or have no embedding, left out). The user's answer about a pair replaces
/// Ollama's. Returns ((a, b), example).
pub fn labelled_examples(db: &Database) -> anyhow::Result<Vec<((i64, i64), crate::pair_model::Example)>> {
    let mut labels: HashMap<(i64, i64), (bool, f32)> = HashMap::new();
    for (a, b, source, same) in db.pair_labels()? {
        let weight = if source == SOURCE_USER { USER_WEIGHT } else { 1.0 };
        let entry = labels.entry((a, b)).or_insert((same, weight));
        if source == SOURCE_USER {
            *entry = (same, weight);
        }
    }
    let ids: Vec<i64> = labels.keys().flat_map(|&(a, b)| [a, b]).collect::<HashSet<_>>().into_iter().collect();
    // Words are weighed against recent stories in general, as when grouping
    let since = chrono::Utc::now() - chrono::Duration::days(crate::embeddings::RECENT_DAYS);
    let stories = db.story_texts(&since.to_rfc3339(), &ids)?;
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

/// Trains the grouping model on every label. `None` if there aren't enough
/// yet. Tests it on a fifth of them first (trained without those), then
/// learns from them all. CPU work: off the UI thread.
pub fn train(db: &Database) -> anyhow::Result<Option<Trained>> {
    use crate::pair_model::{Example, PairModel, Score};
    let examples = labelled_examples(db)?;
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

/// Settings key of what's known about the trained model, for Settings
pub const MODEL_INFO_SETTING: &str = "grouping_model_info";

/// What Settings says about the trained model
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

    pub fn load(db: &Database) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(&db.get_setting(MODEL_INFO_SETTING).ok()??).ok()?;
        Some(Self {
            examples: value["examples"].as_u64()? as usize,
            tested: value["tested"].as_u64()? as usize,
            accuracy: value["accuracy"].as_f64()? as f32,
        })
    }
}

/// Trains the grouping model on every label and, if there are enough,
/// stores it and groups the recent stories again with it. CPU work: off the
/// UI thread.
pub fn train_and_regroup(db: &Database) -> anyhow::Result<Option<ModelInfo>> {
    let Some(trained) = train(db)? else {
        return Ok(None);
    };
    let info = ModelInfo { examples: trained.examples, tested: trained.score.examples, accuracy: trained.score.accuracy() };
    db.set_setting(crate::story_groups::MODEL_SETTING, &trained.model.to_json())?;
    db.set_setting(MODEL_INFO_SETTING, &info.to_json())?;
    crate::story_groups::regroup_recent(db)?;
    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let length = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / length).collect()
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

