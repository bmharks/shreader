//! Tags sort stories into views of their own, out of their folders: a
//! promotions tag catches advertisements, a security tag collects breach
//! news to skim. Each tag has a model learned from the user's answers (see
//! `model`), which works from story embeddings (see `embedder`). The
//! answers, models and results live in the archive (see `store`).
//!
//! Shared by Shreader, which sorts stories with the models and lets the
//! user correct a story's tag, and the trainer (`shreader-train`), where
//! the user reviews stories and trains the models.

pub mod archive;
pub mod embedder;
pub mod model;
pub mod store;
