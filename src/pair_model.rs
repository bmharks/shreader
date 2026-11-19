//! The learned judge of whether two stories are the same news, used to group
//! stories once there are enough labelled examples (from Ollama to start
//! with, then the user's corrections; see `training`).
//!
//! It's a logistic regression: a weighted sum of a few measurements of the
//! pair, turned into a chance between 0 and 1. Small and quick (microseconds
//! a pair, under a second to train on thousands of examples), it learns how
//! much each measurement matters from the examples rather than from rules
//! tuned by hand. The measurements:
//!
//! - how alike their meaning is (embeddings; catches rewording)
//! - how alike their significant words are (TF-IDF; catches names, numbers)
//! - how many significant headline words they share
//! - how far apart in time they are
//! - whether they're from the same feed (a feed's own stories on one topic
//!   are more often separate pieces)

use std::collections::{HashMap, HashSet};

use crate::story_groups::StoryText;

/// Measurements per pair
pub const FEATURES: usize = 5;

/// Pairs less alike in meaning than this are taken to be different news
/// without asking the model: it learned from pairs at least this alike
/// (see `training::candidate_pairs`) and can't speak for others.
pub const MIN_SIMILARITY: f32 = 0.45;

/// A story, with what's needed to compare it
pub struct PairStory<'a> {
    pub story: &'a StoryText,
    pub embedding: &'a [f32],
    /// Its weighted words (see `story_groups::word_vectors`)
    pub words: &'a HashMap<String, f32>,
    /// Its headline's significant words
    pub title: &'a HashSet<String>,
}

/// The pair's measurements, each roughly 0 to 1.
pub fn features(a: &PairStory, b: &PairStory) -> [f32; FEATURES] {
    let shared_title_words = a.title.intersection(b.title).count().min(4) as f32 / 4.0;
    let hours = (a.story.time - b.story.time).abs() as f32 / 3600.0;
    [
        crate::embeddings::similarity(a.embedding, b.embedding),
        crate::story_groups::cosine(a.words, b.words),
        shared_title_words,
        (1.0 + hours).ln() / (1.0 + 72.0f32).ln(),
        (a.story.feed == b.story.feed) as u8 as f32,
    ]
}

/// One labelled pair
#[derive(Debug, Clone)]
pub struct Example {
    pub features: [f32; FEATURES],
    pub same: bool,
    /// How much it counts (the user's own answers count for more)
    pub weight: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairModel {
    /// Each measurement's typical value and spread in the examples it learned
    /// from (measurements are compared on the same footing)
    mean: [f32; FEATURES],
    scale: [f32; FEATURES],
    weights: [f32; FEATURES],
    bias: f32,
}

/// Training passes over the examples
const STEPS: usize = 400;
const LEARNING_RATE: f32 = 0.5;
/// Pulls weights towards 0, so a few odd examples can't make one count
/// for too much
const REGULARIZATION: f32 = 0.001;

impl PairModel {
    /// Learns from `examples`. `None` if there's nothing to learn from (no
    /// examples, or all the same answer).
    pub fn train(examples: &[Example]) -> Option<Self> {
        if !examples.iter().any(|e| e.same) || examples.iter().all(|e| e.same) {
            return None;
        }
        let total_weight: f32 = examples.iter().map(|e| e.weight).sum();
        let mut mean = [0.0; FEATURES];
        let mut scale = [0.0; FEATURES];
        for f in 0..FEATURES {
            mean[f] = examples.iter().map(|e| e.weight * e.features[f]).sum::<f32>() / total_weight;
            let variance =
                examples.iter().map(|e| e.weight * (e.features[f] - mean[f]).powi(2)).sum::<f32>() / total_weight;
            scale[f] = variance.sqrt().max(1e-3);
        }
        let mut model = Self { mean, scale, weights: [0.0; FEATURES], bias: 0.0 };
        let standardized: Vec<[f32; FEATURES]> = examples.iter().map(|e| model.standardize(&e.features)).collect();
        // Gradient descent on the weighted log loss
        for _ in 0..STEPS {
            let mut gradient = [0.0f32; FEATURES];
            let mut bias_gradient = 0.0f32;
            for (example, x) in examples.iter().zip(&standardized) {
                let error = example.weight * (sigmoid(model.score(x)) - example.same as u8 as f32);
                for f in 0..FEATURES {
                    gradient[f] += error * x[f];
                }
                bias_gradient += error;
            }
            for f in 0..FEATURES {
                model.weights[f] -= LEARNING_RATE * (gradient[f] / total_weight + REGULARIZATION * model.weights[f]);
            }
            model.bias -= LEARNING_RATE * bias_gradient / total_weight;
        }
        Some(model)
    }

    fn standardize(&self, features: &[f32; FEATURES]) -> [f32; FEATURES] {
        std::array::from_fn(|f| (features[f] - self.mean[f]) / self.scale[f])
    }

    fn score(&self, standardized: &[f32; FEATURES]) -> f32 {
        self.bias + self.weights.iter().zip(standardized).map(|(w, x)| w * x).sum::<f32>()
    }

    /// The chance (0 to 1) that a pair with these measurements is the same news
    pub fn probability(&self, features: &[f32; FEATURES]) -> f32 {
        sigmoid(self.score(&self.standardize(features)))
    }

    /// As `probability`, but 0 for pairs too unlike in meaning to ask about
    pub fn probability_if_close(&self, a: &PairStory, b: &PairStory) -> f32 {
        if crate::embeddings::similarity(a.embedding, b.embedding) < MIN_SIMILARITY {
            return 0.0;
        }
        self.probability(&features(a, b))
    }

    /// As stored (in the settings)
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "features": FEATURES,
            "mean": self.mean,
            "scale": self.scale,
            "weights": self.weights,
            "bias": self.bias,
        })
        .to_string()
    }

    /// From `to_json`'s output; `None` if it isn't one (or is from a version
    /// with other measurements)
    pub fn from_json(json: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        if value["features"].as_u64()? != FEATURES as u64 {
            return None;
        }
        let array = |key: &str| -> Option<[f32; FEATURES]> {
            let values = value[key].as_array()?;
            if values.len() != FEATURES {
                return None;
            }
            let mut out = [0.0; FEATURES];
            for (slot, v) in out.iter_mut().zip(values) {
                *slot = v.as_f64()? as f32;
            }
            out.iter().all(|x| x.is_finite()).then_some(out)
        };
        let bias = value["bias"].as_f64()? as f32;
        let scale = array("scale")?;
        if !bias.is_finite() || scale.iter().any(|s| *s <= 0.0) {
            return None;
        }
        Some(Self { mean: array("mean")?, scale, weights: array("weights")?, bias })
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// How a model does on examples it didn't learn from
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Score {
    pub examples: usize,
    pub correct: usize,
    /// Of the pairs it called the same, how many were
    pub precision: f32,
    /// Of the pairs that were the same, how many it called the same
    pub recall: f32,
}

impl Score {
    pub fn of(judged: impl Iterator<Item = (bool, bool)>) -> Self {
        let (mut examples, mut correct, mut said_same, mut were_same, mut both) = (0, 0, 0, 0, 0);
        for (said, was) in judged {
            examples += 1;
            correct += (said == was) as usize;
            said_same += said as usize;
            were_same += was as usize;
            both += (said && was) as usize;
        }
        let share = |n: usize, d: usize| if d == 0 { 0.0 } else { n as f32 / d as f32 };
        Self { examples, correct, precision: share(both, said_same), recall: share(both, were_same) }
    }

    pub fn accuracy(&self) -> f32 {
        if self.examples == 0 { 0.0 } else { self.correct as f32 / self.examples as f32 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same news: alike in meaning and words; different: not
    fn examples() -> Vec<Example> {
        let mut examples = Vec::new();
        for i in 0..50 {
            let jitter = (i % 7) as f32 * 0.02;
            examples.push(Example { features: [0.8 + jitter, 0.5 + jitter, 0.5, 0.3, 0.0], same: true, weight: 1.0 });
            examples.push(Example { features: [0.55 + jitter, 0.1 + jitter, 0.0, 0.6, 1.0], same: false, weight: 1.0 });
        }
        examples
    }

    #[test]
    fn learns_to_tell_same_from_different() {
        let model = PairModel::train(&examples()).unwrap();
        assert!(model.probability(&[0.85, 0.55, 0.5, 0.3, 0.0]) > 0.9);
        assert!(model.probability(&[0.55, 0.1, 0.0, 0.6, 1.0]) < 0.1);
        let score = Score::of(examples().iter().map(|e| (model.probability(&e.features) >= 0.5, e.same)));
        assert_eq!(score.accuracy(), 1.0);
    }

    #[test]
    fn nothing_to_learn_from_one_kind_of_answer() {
        let only_same: Vec<Example> = examples().into_iter().filter(|e| e.same).collect();
        assert!(PairModel::train(&only_same).is_none());
        assert!(PairModel::train(&[]).is_none());
    }

    #[test]
    fn stored_and_loaded() {
        let model = PairModel::train(&examples()).unwrap();
        assert_eq!(PairModel::from_json(&model.to_json()), Some(model));
        assert_eq!(PairModel::from_json("{}"), None);
        assert_eq!(PairModel::from_json(r#"{"features":3,"mean":[0,0,0]}"#), None);
    }

    #[test]
    fn weighted_examples_count_for_more() {
        // Two contradicting answers about the same pair: the weightier wins
        let pair = [0.6, 0.3, 0.25, 0.4, 0.0];
        let mut examples = examples();
        examples.push(Example { features: pair, same: true, weight: 1.0 });
        examples.push(Example { features: pair, same: false, weight: 1.0 });
        let even = PairModel::train(&examples).unwrap().probability(&pair);
        examples.last_mut().unwrap().weight = 30.0;
        let weighted = PairModel::train(&examples).unwrap().probability(&pair);
        assert!(weighted < even, "{} vs {}", weighted, even);
    }
}
