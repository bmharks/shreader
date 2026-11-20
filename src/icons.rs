//! Small SVG icons.
//!
//! iced's default font has no emoji and few symbols, so "📁", "★" and the like
//! render as empty boxes. These are drawn as SVG instead and tinted to match
//! the surrounding text, so they look the same on every system.

use std::sync::LazyLock;

use iced::widget::svg::{self, Svg};
use iced::{Color, Length, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    Star,
    StarFilled,
    Refresh,
    ExternalLink,
    Settings,
    Tag,
}

// 24x24 outline icons, stroke-based. The stroke color is irrelevant: the
// widget recolors every pixel with the tint from `style`.
const FOLDER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>"#;
const STAR_PATH: &str = "M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z";
const REFRESH: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16"/><path d="M16 16h5v5"/></svg>"#;
const EXTERNAL_LINK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/></svg>"#;

const SETTINGS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>"#;

const TAG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z"/><circle cx="7.5" cy="7.5" r="1" fill="black"/></svg>"#;

fn star(filled: bool) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="{}" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="{}"/></svg>"#,
        if filled { "black" } else { "none" },
        STAR_PATH
    )
}

// Built once: a handle made from bytes is hashed on creation, and views are
// rebuilt on every message.
static FOLDER_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(FOLDER.as_bytes()));
static STAR_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(star(false).into_bytes()));
static STAR_FILLED_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(star(true).into_bytes()));
static REFRESH_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(REFRESH.as_bytes()));
static SETTINGS_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(SETTINGS.as_bytes()));
static TAG_H: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(TAG.as_bytes()));
static EXTERNAL_LINK_H: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(EXTERNAL_LINK.as_bytes()));

/// Which color an icon takes, to match the text next to it.
#[derive(Debug, Clone, Copy)]
pub enum Tint {
    /// Regular text
    Text,
    /// Dimmed text (`dim_text`)
    Secondary,
    /// Text on a `button::primary` background
    OnPrimary,
    /// Text on a `button::secondary` background
    OnSecondary,
    /// The gold of a set star
    Gold,
}

impl Tint {
    pub(crate) fn color(self, theme: &Theme) -> Color {
        let palette = theme.extended_palette();
        match self {
            Tint::Text => palette.background.base.text,
            Tint::Secondary => dim_color(theme),
            // `button::primary` draws on `primary.strong`; `primary.base.text` is black
            Tint::OnPrimary => palette.primary.strong.text,
            Tint::OnSecondary => palette.secondary.base.text,
            Tint::Gold => Color::from_rgb8(0xF5, 0xB4, 0x2A),
        }
    }
}

/// Color of dimmed text and icons: iced's `secondary.strong`, except on a
/// light theme, where that grey is too faint to read on white.
pub fn dim_color(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    if palette.is_dark {
        palette.secondary.strong.color
    } else {
        Color::from_rgb(0.40, 0.40, 0.43)
    }
}

/// Style for dimmed text (dates, notes, counts), in place of
/// `text::secondary`, which is too faint on the light theme.
pub fn dim_text(theme: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style { color: Some(dim_color(theme)) }
}

/// A square icon of `size` pixels, tinted with `tint`.
pub fn icon<'a>(icon: Icon, size: f32, tint: Tint) -> Svg<'a> {
    Svg::new(handle(icon))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |theme: &Theme, _status| svg::Style {
            color: Some(tint.color(theme)),
        })
}

pub(crate) fn handle(icon: Icon) -> svg::Handle {
    let handle = match icon {
        Icon::Folder => &*FOLDER_H,
        Icon::Star => &*STAR_H,
        Icon::StarFilled => &*STAR_FILLED_H,
        Icon::Refresh => &*REFRESH_H,
        Icon::ExternalLink => &*EXTERNAL_LINK_H,
        Icon::Settings => &*SETTINGS_H,
        Icon::Tag => &*TAG_H,
    };
    handle.clone()
}
