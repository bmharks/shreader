//! A tag's model: whether a story belongs in the tag (a promotion, security
//! news...), from its embedding (see `embedder`). Learned from the user's
//! answers about single stories.
//!
//! Two kinds are trained, and whichever does better on stories it didn't
//! learn from is kept:
//!
//! - **Contrastive**: a small network first learns to reshape the
//!   embeddings so stories in the tag sit close together and far from
//!   everything else. It learns from pairs: two tagged stories should come
//!   out alike, a tagged and an untagged one unalike. Pairs that look alike
//!   but have different answers (a promotion and a real story, often from
//!   the same site) are picked on purpose, since they're what the model has
//!   to tell apart. Then a classifier draws the line in the reshaped space.
//!   This is the idea behind SetFit, which does well with a few dozen
//!   examples; here the language model itself stays as it is, and only
//!   the small network on top is trained, which takes seconds on a CPU.
//! - **Plain**: the classifier alone, on the embeddings as they are. It
//!   needs fewer examples, and with only a few it often does as well. On
//!   1,761 tech stories labelled as promotions or not (by Ollama), the two
//!   came out about even at every number of answers from 100 up; the
//!   stronger form of contrastive training retrains the language model
//!   itself, which would also need each tag to embed stories separately.
//!
//! Each is judged by cross-validation: trained on four fifths of the
//! answers and tried on the rest, five times over, so every answer is
//! predicted once by a model that didn't see it. The same predictions
//! decide how sure a model must be before it tags a story: precision counts
//! for more than recall, as wrongly hiding a real story costs more than
//! missing a promotion.

use anyhow::{Result, bail, ensure};
use candle_core::{D, DType, Device, Tensor, Var};
use candle_nn::{AdamW, Optimizer, ParamsAdamW};

use crate::embedder::DIMENSIONS;

/// One labelled story
#[derive(Debug, Clone)]
pub struct Example {
    pub embedding: Vec<f32>,
    pub is_tag: bool,
    /// Its feed (stories from the same feed make the hardest pairs)
    pub feed: i64,
}

/// The fewest answers of each kind (in the tag, not in it) to train on
pub const MIN_EACH: usize = 4;

/// Hidden size of the contrastive network; its output is an embedding
const HIDDEN: usize = 96;
const OUTPUT: usize = DIMENSIONS;

/// Training steps and pace for the network, and for the classifier
const ADAPTER_STEPS: usize = 40;
const ADAPTER_RATE: f64 = 0.003;
const HEAD_STEPS: usize = 300;
const HEAD_RATE: f64 = 0.05;
/// Pulls weights towards 0, so a few odd examples can't count for too much
const WEIGHT_DECAY: f64 = 0.01;
/// Stories drawn per step to pair up for the network (half in the tag)
const ANCHORS: usize = 256;
/// Unalike candidates looked at for each hard pair
const HARD_CANDIDATES: usize = 8;

/// How many parts the answers are split into for cross-validation
const FOLDS: usize = 5;

/// How much recall counts against precision when choosing how sure a model
/// must be: the square of the F-score's beta (0.5, which weights precision
/// more)
const BETA_SQUARED: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Contrastive,
    Plain,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Contrastive => "contrastive",
            Kind::Plain => "plain",
        }
    }
}

/// The network that reshapes embeddings (contrastive models only): it adds
/// an adjustment to the embedding, x + tanh(x·w1 + b1)·w2 + b2, scaled to
/// length 1. The adjustment starts at nothing (w2 is 0), so training starts
/// from the language model's own view of the stories and only changes what
/// the answers show needs changing.
#[derive(Debug, Clone, PartialEq)]
struct Adapter {
    /// DIMENSIONS × HIDDEN, row by row
    w1: Vec<f32>,
    b1: Vec<f32>,
    /// HIDDEN × OUTPUT
    w2: Vec<f32>,
    b2: Vec<f32>,
}

impl Adapter {
    fn apply(&self, x: &[f32]) -> Vec<f32> {
        let mut hidden = self.b1.clone();
        for (d, &v) in x.iter().enumerate() {
            if v != 0.0 {
                let row = &self.w1[d * HIDDEN..(d + 1) * HIDDEN];
                hidden.iter_mut().zip(row).for_each(|(h, w)| *h += v * w);
            }
        }
        let mut out: Vec<f32> = x.iter().zip(&self.b2).map(|(a, b)| a + b).collect();
        for (h, &v) in hidden.iter().enumerate() {
            let v = v.tanh();
            let row = &self.w2[h * OUTPUT..(h + 1) * OUTPUT];
            out.iter_mut().zip(row).for_each(|(o, w)| *o += v * w);
        }
        normalize(&mut out);
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagModel {
    adapter: Option<Adapter>,
    weights: Vec<f32>,
    bias: f32,
    /// How sure it must be to tag a story (0 to 1)
    pub threshold: f32,
}

/// How a model did on answers it didn't learn from
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub kind: Kind,
    pub positives: usize,
    pub negatives: usize,
    /// Tagged stories it would catch
    pub caught: usize,
    /// Untagged stories it would wrongly tag
    pub wrongly_tagged: usize,
    pub threshold: f32,
}

impl Evaluation {
    /// Of the stories it tags, the share that should be
    pub fn precision(&self) -> f32 {
        let tagged = self.caught + self.wrongly_tagged;
        if tagged == 0 { 0.0 } else { self.caught as f32 / tagged as f32 }
    }

    /// Of the stories that should be tagged, the share it tags
    pub fn recall(&self) -> f32 {
        if self.positives == 0 { 0.0 } else { self.caught as f32 / self.positives as f32 }
    }

    /// One number to compare models by (the F-score weighted to precision)
    pub fn score(&self) -> f32 {
        f_score(self.precision(), self.recall())
    }

    /// For people: e.g. "catches 41 of 45 (91%); wrongly tags 2 of 380"
    pub fn describe(&self) -> String {
        format!(
            "catches {} of {} ({:.0}%); wrongly tags {} of {}",
            self.caught,
            self.positives,
            self.recall() * 100.0,
            self.wrongly_tagged,
            self.negatives
        )
    }
}

fn f_score(precision: f32, recall: f32) -> f32 {
    if precision + recall == 0.0 {
        return 0.0;
    }
    (1.0 + BETA_SQUARED) * precision * recall / (BETA_SQUARED * precision + recall)
}

/// The result of training
#[derive(Debug, Clone)]
pub struct Trained {
    /// Trained on every answer, of the kind that did better
    pub model: TagModel,
    /// How each kind did (the kept one first)
    pub evaluations: Vec<Evaluation>,
}

impl TagModel {
    /// How likely the story is in the tag (0 to 1)
    pub fn score(&self, embedding: &[f32]) -> f32 {
        let adapted;
        let x = match &self.adapter {
            Some(adapter) => {
                adapted = adapter.apply(embedding);
                &adapted[..]
            }
            None => embedding,
        };
        let z: f32 = self.bias + x.iter().zip(&self.weights).map(|(a, b)| a * b).sum::<f32>();
        1.0 / (1.0 + (-z).exp())
    }

    /// Whether the story is in the tag
    pub fn tags(&self, embedding: &[f32]) -> bool {
        embedding.len() == DIMENSIONS && self.score(embedding) >= self.threshold
    }

    pub fn kind(&self) -> Kind {
        if self.adapter.is_some() { Kind::Contrastive } else { Kind::Plain }
    }

    /// Learns from `examples`: both kinds are cross-validated, and the
    /// better one is trained on everything. Takes seconds: off the UI thread.
    pub fn train(examples: &[Example]) -> Result<Trained> {
        let positives = examples.iter().filter(|e| e.is_tag).count();
        let negatives = examples.len() - positives;
        ensure!(
            positives >= MIN_EACH && negatives >= MIN_EACH,
            "needs at least {} stories in the tag and {} not in it (has {} and {})",
            MIN_EACH,
            MIN_EACH,
            positives,
            negatives
        );
        ensure!(examples.iter().all(|e| e.embedding.len() == DIMENSIONS), "an embedding has the wrong size");
        let mut evaluations: Vec<Evaluation> = [Kind::Contrastive, Kind::Plain]
            .into_iter()
            .map(|kind| cross_validate(examples, kind))
            .collect::<Result<_>>()?;
        // The better first; on a tie, the simpler
        evaluations.sort_by(|a, b| {
            b.score().total_cmp(&a.score()).then((a.kind == Kind::Contrastive).cmp(&(b.kind == Kind::Contrastive)))
        });
        let best = &evaluations[0];
        let mut model = fit(examples, best.kind, &mut Rng::new(1))?;
        model.threshold = best.threshold;
        Ok(Trained { model, evaluations })
    }

    /// As stored: a short header, then the numbers (little-endian f32s)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(self.adapter.is_some() as u8);
        let mut put = |values: &[f32]| values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        put(&[self.threshold, self.bias]);
        if let Some(a) = &self.adapter {
            put(&a.w1);
            put(&a.b1);
            put(&a.w2);
            put(&a.b2);
        }
        put(&self.weights);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let Some(rest) = bytes.strip_prefix(MAGIC) else { bail!("not a tag model") };
        let Some((&contrastive, rest)) = rest.split_first() else { bail!("tag model cut short") };
        let values: Vec<f32> = rest.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let input = if contrastive == 1 { OUTPUT } else { DIMENSIONS };
        let adapter_len = if contrastive == 1 { DIMENSIONS * HIDDEN + HIDDEN + HIDDEN * OUTPUT + OUTPUT } else { 0 };
        ensure!(contrastive <= 1 && rest.len() % 4 == 0, "not a tag model");
        ensure!(values.len() == 2 + adapter_len + input, "tag model has the wrong size");
        let mut at = 2;
        let mut take = |n: usize| {
            at += n;
            values[at - n..at].to_vec()
        };
        let adapter = (contrastive == 1).then(|| Adapter {
            w1: take(DIMENSIONS * HIDDEN),
            b1: take(HIDDEN),
            w2: take(HIDDEN * OUTPUT),
            b2: take(OUTPUT),
        });
        let weights = take(input);
        Ok(Self { adapter, weights, bias: values[1], threshold: values[0] })
    }
}

const MAGIC: &[u8] = b"SHTAG1";

/// Predicts every example with a model that didn't learn from it, and finds
/// the threshold that does best on those predictions.
fn cross_validate(examples: &[Example], kind: Kind) -> Result<Evaluation> {
    let mut rng = Rng::new(7);
    // Stratified: each part gets its share of tagged stories
    let mut fold = vec![0usize; examples.len()];
    for want in [true, false] {
        let mut ids: Vec<usize> = (0..examples.len()).filter(|&i| examples[i].is_tag == want).collect();
        rng.shuffle(&mut ids);
        for (n, i) in ids.into_iter().enumerate() {
            fold[i] = n % FOLDS;
        }
    }
    let mut scores = vec![0.0f32; examples.len()];
    for f in 0..FOLDS {
        let train: Vec<Example> = (0..examples.len()).filter(|&i| fold[i] != f).map(|i| examples[i].clone()).collect();
        let model = fit(&train, kind, &mut rng)?;
        for i in (0..examples.len()).filter(|&i| fold[i] == f) {
            scores[i] = model.score(&examples[i].embedding);
        }
    }
    Ok(best_threshold(examples, &scores, kind))
}

/// The threshold with the best (precision-weighted) score on `scores`
fn best_threshold(examples: &[Example], scores: &[f32], kind: Kind) -> Evaluation {
    let positives = examples.iter().filter(|e| e.is_tag).count();
    let negatives = examples.len() - positives;
    let mut order: Vec<usize> = (0..examples.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
    // Tagging the n most likely, for each n; ties are taken together
    let mut best = Evaluation { kind, positives, negatives, caught: 0, wrongly_tagged: 0, threshold: 1.0 };
    let (mut caught, mut wrong) = (0, 0);
    for (n, &i) in order.iter().enumerate() {
        if examples[i].is_tag { caught += 1 } else { wrong += 1 }
        if order.get(n + 1).is_some_and(|&next| scores[next] == scores[i]) {
            continue;
        }
        let candidate = Evaluation { kind, positives, negatives, caught, wrongly_tagged: wrong, threshold: scores[i] };
        if candidate.score() > best.score() {
            best = candidate;
        }
    }
    // Halfway to the next story down, so a slightly less sure story still
    // counts (and the line isn't drawn exactly on one example)
    if let Some(pos) = order.iter().position(|&i| scores[i] < best.threshold) {
        best.threshold = (best.threshold + scores[order[pos]]) / 2.0;
    }
    best
}

/// Trains one model of `kind` on `examples` (its threshold is left at 0.5)
fn fit(examples: &[Example], kind: Kind, rng: &mut Rng) -> Result<TagModel> {
    let device = Device::Cpu;
    let n = examples.len();
    let flat: Vec<f32> = examples.iter().flat_map(|e| e.embedding.iter().copied()).collect();
    let x = Tensor::from_vec(flat, (n, DIMENSIONS), &device)?;
    let adapter = match kind {
        Kind::Contrastive => Some(train_adapter(examples, &x, rng)?),
        Kind::Plain => None,
    };
    let inputs: Vec<Vec<f32>> = match &adapter {
        Some(a) => examples.iter().map(|e| a.apply(&e.embedding)).collect(),
        None => examples.iter().map(|e| e.embedding.clone()).collect(),
    };
    let labels: Vec<bool> = examples.iter().map(|e| e.is_tag).collect();
    let (weights, bias) = train_head(&inputs, &labels, rng)?;
    Ok(TagModel { adapter, weights, bias, threshold: 0.5 })
}

/// Learns the network that reshapes embeddings, from pairs: alike (both in
/// the tag, or both not) should come out with a similarity of 1, unalike 0.
fn train_adapter(examples: &[Example], x: &Tensor, rng: &mut Rng) -> Result<Adapter> {
    let device = x.device();
    let w1 = Var::from_vec(rng.weights(DIMENSIONS * HIDDEN, DIMENSIONS), (DIMENSIONS, HIDDEN), device)?;
    let b1 = Var::zeros(HIDDEN, DType::F32, device)?;
    let w2 = Var::zeros((HIDDEN, OUTPUT), DType::F32, device)?;
    let b2 = Var::zeros(OUTPUT, DType::F32, device)?;
    let params = ParamsAdamW { lr: ADAPTER_RATE, weight_decay: WEIGHT_DECAY, ..Default::default() };
    let mut optimizer = AdamW::new(vec![w1.clone(), b1.clone(), w2.clone(), b2.clone()], params)?;

    let ids_in: Vec<usize> = (0..examples.len()).filter(|&i| examples[i].is_tag).collect();
    let ids_out: Vec<usize> = (0..examples.len()).filter(|&i| !examples[i].is_tag).collect();
    for _ in 0..ADAPTER_STEPS {
        let (mut a, mut b, mut y) = (Vec::new(), Vec::new(), Vec::new());
        for k in 0..ANCHORS {
            let (same, other) = if k % 2 == 0 { (&ids_in, &ids_out) } else { (&ids_out, &ids_in) };
            let anchor = same[rng.below(same.len())];
            // Alike: another from the same side
            let mut partner = same[rng.below(same.len())];
            if partner == anchor && same.len() > 1 {
                partner = same[(same.iter().position(|&i| i == anchor).unwrap() + 1) % same.len()];
            }
            a.push(anchor as u32);
            b.push(partner as u32);
            y.push(1.0f32);
            // Unalike: half the time the most similar of a few candidates
            // (from the same feed counts as more similar)
            let unalike = if rng.below(2) == 0 {
                (0..HARD_CANDIDATES)
                    .map(|_| other[rng.below(other.len())])
                    .max_by(|&p, &q| closeness(&examples[anchor], &examples[p]).total_cmp(&closeness(&examples[anchor], &examples[q])))
                    .unwrap()
            } else {
                other[rng.below(other.len())]
            };
            a.push(anchor as u32);
            b.push(unalike as u32);
            y.push(0.0);
        }
        let pairs = a.len();
        let a = Tensor::from_vec(a, pairs, device)?;
        let b = Tensor::from_vec(b, pairs, device)?;
        let y = Tensor::from_vec(y, pairs, device)?;
        let out = adapt(x, &w1, &b1, &w2, &b2)?;
        let similarity = (out.index_select(&a, 0)? * out.index_select(&b, 0)?)?.sum(D::Minus1)?;
        let loss = (similarity - y)?.sqr()?.mean_all()?;
        optimizer.backward_step(&loss)?;
    }
    Ok(Adapter {
        w1: w1.as_tensor().flatten_all()?.to_vec1()?,
        b1: b1.as_tensor().to_vec1()?,
        w2: w2.as_tensor().flatten_all()?.to_vec1()?,
        b2: b2.as_tensor().to_vec1()?,
    })
}

/// The adapter's forward pass, for training (same as `Adapter::apply`)
fn adapt(x: &Tensor, w1: &Var, b1: &Var, w2: &Var, b2: &Var) -> Result<Tensor> {
    let hidden = x.matmul(w1.as_tensor())?.broadcast_add(b1.as_tensor())?.tanh()?;
    let out = (x + hidden.matmul(w2.as_tensor())?.broadcast_add(b2.as_tensor())?)?;
    let length = out.sqr()?.sum_keepdim(D::Minus1)?.sqrt()?.affine(1.0, 1e-6)?;
    Ok(out.broadcast_div(&length)?)
}

/// How alike two stories look before training (for picking hard pairs)
fn closeness(a: &Example, b: &Example) -> f32 {
    crate::embedder::similarity(&a.embedding, &b.embedding) + if a.feed == b.feed { 0.1 } else { 0.0 }
}

/// Logistic regression: weights and bias. Each side counts equally in
/// total, however few stories are in the tag.
fn train_head(inputs: &[Vec<f32>], labels: &[bool], rng: &mut Rng) -> Result<(Vec<f32>, f32)> {
    let device = Device::Cpu;
    let n = inputs.len();
    let dims = inputs[0].len();
    let x = Tensor::from_vec(inputs.concat(), (n, dims), &device)?;
    let positives = labels.iter().filter(|&&l| l).count() as f32;
    let negatives = n as f32 - positives;
    let y = Tensor::from_vec(labels.iter().map(|&l| l as u8 as f32).collect::<Vec<_>>(), n, &device)?;
    let balance: Vec<f32> =
        labels.iter().map(|&l| if l { n as f32 / (2.0 * positives) } else { n as f32 / (2.0 * negatives) }).collect();
    let balance = Tensor::from_vec(balance, n, &device)?;
    let w = Var::from_vec(rng.weights(dims, dims), (dims, 1), &device)?;
    let b = Var::zeros(1, DType::F32, &device)?;
    let params = ParamsAdamW { lr: HEAD_RATE, weight_decay: WEIGHT_DECAY, ..Default::default() };
    let mut optimizer = AdamW::new(vec![w.clone(), b.clone()], params)?;
    for _ in 0..HEAD_STEPS {
        let z = x.matmul(w.as_tensor())?.squeeze(1)?.broadcast_add(b.as_tensor())?;
        // log(1 + e^z) - y·z, written so large z can't overflow
        let softplus = (z.relu()? + z.abs()?.neg()?.exp()?.affine(1.0, 1.0)?.log()?)?;
        let loss = ((softplus - (&y * &z)?)? * &balance)?.mean_all()?;
        optimizer.backward_step(&loss)?;
    }
    Ok((w.as_tensor().flatten_all()?.to_vec1()?, b.as_tensor().to_vec1::<f32>()?[0]))
}

fn normalize(v: &mut [f32]) {
    let length = v.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-6;
    v.iter_mut().for_each(|x| *x /= length);
}

/// A small seeded random number generator (xorshift*), so training gives
/// the same model from the same answers
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Between -1 and 1
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }

    /// Starting weights for a layer with `inputs` inputs (Xavier-style)
    fn weights(&mut self, n: usize, inputs: usize) -> Vec<f32> {
        let scale = (3.0 / inputs as f32).sqrt();
        (0..n).map(|_| self.unit() * scale).collect()
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector near `centre` (one of a few directions), with noise
    fn story(rng: &mut Rng, centre: usize, noise: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..DIMENSIONS).map(|_| rng.unit() * noise).collect();
        v[centre] += 1.0;
        normalize(&mut v);
        v
    }

    /// Promotions are near two directions (two kinds of promotion); other
    /// stories near any of eight others
    fn examples(n: usize, seed: u64) -> Vec<Example> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|i| {
                let is_tag = i % 5 == 0;
                let centre = if is_tag { [0, 1][i % 2] } else { 2 + rng.below(8) };
                Example { embedding: story(&mut rng, centre, 0.06), is_tag, feed: (i % 7) as i64 }
            })
            .collect()
    }

    #[test]
    fn learns_a_tag_and_says_how_well() {
        let trained = TagModel::train(&examples(200, 3)).unwrap();
        assert_eq!(trained.evaluations.len(), 2);
        let kept = &trained.evaluations[0];
        assert_eq!(kept.kind, trained.model.kind());
        assert_eq!((kept.positives, kept.negatives), (40, 160));
        assert!(kept.precision() > 0.9 && kept.recall() > 0.9, "{}", kept.describe());
        // On new stories too
        let fresh = examples(100, 99);
        let right = fresh.iter().filter(|e| trained.model.tags(&e.embedding) == e.is_tag).count();
        assert!(right >= 95, "{} of 100 right", right);
    }

    #[test]
    fn contrastive_models_learn_and_store() {
        let data = examples(120, 5);
        let mut model = fit(&data, Kind::Contrastive, &mut Rng::new(1)).unwrap();
        model.threshold = 0.5;
        let right = data.iter().filter(|e| model.tags(&e.embedding) == e.is_tag).count();
        assert!(right >= 115, "{} of 120 right", right);
        let back = TagModel::from_bytes(&model.to_bytes()).unwrap();
        assert_eq!(back, model);
        assert_eq!(back.kind(), Kind::Contrastive);
        let plain = fit(&data, Kind::Plain, &mut Rng::new(1)).unwrap();
        assert_eq!(TagModel::from_bytes(&plain.to_bytes()).unwrap(), plain);
        assert!(TagModel::from_bytes(b"SHTAG1\x01\x00").is_err());
        assert!(TagModel::from_bytes(b"nonsense").is_err());
    }

    #[test]
    fn training_is_repeatable() {
        let data = examples(60, 8);
        let a = TagModel::train(&data).unwrap();
        let b = TagModel::train(&data).unwrap();
        assert_eq!(a.model, b.model);
    }

    #[test]
    fn needs_examples_of_both() {
        let mut data = examples(60, 2);
        data.retain(|e| !e.is_tag);
        data.extend(examples(20, 2).into_iter().filter(|e| e.is_tag).take(3));
        let err = TagModel::train(&data).unwrap_err().to_string();
        assert!(err.contains("has 3 and"), "{}", err);
    }

    #[test]
    fn threshold_favours_precision() {
        // Scores: two sure positives, then a negative and a positive tied
        let mk = |is_tag| Example { embedding: vec![], is_tag, feed: 0 };
        let data = [mk(true), mk(true), mk(false), mk(true), mk(false)];
        let e = best_threshold(&data, &[0.9, 0.8, 0.6, 0.6, 0.1], Kind::Plain);
        assert_eq!((e.caught, e.wrongly_tagged), (2, 0));
        assert!(e.threshold > 0.6 && e.threshold < 0.8, "{}", e.threshold);
    }
}
