//! Shreader's trainer: teaches tags (such as Promotions) which stories are
//! theirs. You answer whether stories are in a tag, it trains the tag's
//! model, and Shreader then sorts stories with it. Works on Shreader's
//! archive, alongside Shreader (see `archive`).

mod app;
mod archive;
mod queue;

use archive::Archive;

fn main() -> iced::Result {
    let archive = match Archive::default_path().and_then(|path| Archive::open(&path)) {
        Ok(archive) => archive,
        Err(e) => {
            eprintln!("shreader-train: {:#}", e);
            std::process::exit(1);
        }
    };
    iced::application("Shreader Trainer", app::Trainer::update, app::Trainer::view)
        .subscription(app::Trainer::subscription)
        .theme(app::Trainer::theme)
        .window_size((1100.0, 760.0))
        .run_with(move || app::Trainer::new(archive))
}
