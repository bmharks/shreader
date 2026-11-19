//! Which story to review next. Each answer teaches the model most when it's
//! about a story the model can't decide on, so that's the usual order. Early
//! on, with too few answers for a model, stories like the ones already said
//! to be in the tag come first instead, since a tag's stories are usually
//! rare (promotions are around one story in ten).

use shreader_tags::embedder::similarity;
use shreader_tags::model::TagModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Closest to the model's line first
    LeastSure,
    /// Most likely in the tag first
    MostLikely,
    Newest,
}

impl Order {
    pub const ALL: [Order; 3] = [Order::LeastSure, Order::MostLikely, Order::Newest];
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Order::LeastSure => "Least sure first",
            Order::MostLikely => "Most likely first",
            Order::Newest => "Newest first",
        })
    }
}

/// The stories in `candidates` (newest first) in `order`. Without a model,
/// both model orders put the stories most like `positives` first, and with
/// neither, newest first.
pub fn arrange(candidates: &[(i64, Vec<f32>)], order: Order, model: Option<&TagModel>, positives: &[Vec<f32>]) -> Vec<i64> {
    let ids = || candidates.iter().map(|(id, _)| *id).collect();
    if order == Order::Newest {
        return ids();
    }
    // Lower comes first
    let rank: Box<dyn Fn(&[f32]) -> f32> = match (model, order) {
        (Some(m), Order::LeastSure) => Box::new(move |e| (m.score(e) - m.threshold).abs()),
        (Some(m), _) => Box::new(move |e| -m.score(e)),
        (None, _) if !positives.is_empty() => {
            Box::new(move |e| -positives.iter().map(|p| similarity(p, e)).fold(f32::MIN, f32::max))
        }
        (None, _) => return ids(),
    };
    let mut ranked: Vec<(f32, usize)> = candidates.iter().enumerate().map(|(n, (_, e))| (rank(e), n)).collect();
    // Stable: equal ranks stay newest first
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    ranked.into_iter().map(|(_, n)| candidates[n].0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shreader_tags::embedder::DIMENSIONS;
    use shreader_tags::model::Example;

    fn unit(axis: usize, lean: f32) -> Vec<f32> {
        let mut v = vec![0.0; DIMENSIONS];
        v[axis] = 1.0;
        v[axis + 1] = lean;
        let length = (1.0 + lean * lean).sqrt();
        v.iter().map(|x| x / length).collect()
    }

    #[test]
    fn without_a_model_stories_like_the_tags_come_first() {
        let candidates = vec![(1, unit(5, 0.0)), (2, unit(0, 0.5)), (3, unit(0, 0.1))];
        assert_eq!(arrange(&candidates, Order::LeastSure, None, &[]), [1, 2, 3]);
        assert_eq!(arrange(&candidates, Order::MostLikely, None, &[unit(0, 0.0)]), [3, 2, 1]);
        assert_eq!(arrange(&candidates, Order::Newest, None, &[unit(0, 0.0)]), [1, 2, 3]);
    }

    #[test]
    fn with_a_model_the_least_sure_come_first() {
        // In the tag: near axis 0; not: near axis 10. Story 2 is between.
        let mut examples = Vec::new();
        for n in 0..12 {
            let lean = n as f32 * 0.05;
            examples.push(Example { embedding: unit(0, lean), is_tag: true, feed: 1 });
            examples.push(Example { embedding: unit(10, lean), is_tag: false, feed: 1 });
            examples.push(Example { embedding: unit(20, lean), is_tag: false, feed: 1 });
        }
        let model = TagModel::train(&examples).unwrap().model;
        let mut between: Vec<f32> = unit(0, 0.0).iter().zip(unit(10, 0.0)).map(|(a, b)| a + b).collect();
        let length = between.iter().map(|x| x * x).sum::<f32>().sqrt();
        between.iter_mut().for_each(|x| *x /= length);
        let candidates = vec![(1, unit(10, 0.2)), (2, between), (3, unit(0, 0.2))];
        assert_eq!(arrange(&candidates, Order::LeastSure, Some(&model), &[])[0], 2);
        assert_eq!(arrange(&candidates, Order::MostLikely, Some(&model), &[]), [3, 2, 1]);
    }
}
