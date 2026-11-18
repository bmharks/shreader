//! How Shreader fits into the desktop: its window settings and icon, the
//! system tray icon (so it can keep running, and refreshing, with its window
//! closed), and making sure only one copy runs.
//!
//! The tray and a second copy of the app talk to the running app through a
//! channel of `Command`s, which the app receives as a subscription
//! (`commands`).

use std::sync::{Mutex, OnceLock};

use futures::stream::{self, Stream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// The app ID: the launcher file is `<APP_ID>.desktop` and the icon
/// `<APP_ID>.svg` (see `packaging/`). Desktops match windows to them by it.
/// Also the D-Bus name the running copy holds.
pub const APP_ID: &str = "io.github.bmharks.Shreader";

/// D-Bus object through which a second copy asks the running one to show itself
const DBUS_PATH: &str = "/io/github/bmharks/Shreader";

/// The app icon (also installed with the launcher)
const ICON_SVG: &str = include_str!("../packaging/io.github.bmharks.Shreader.svg");

/// Window settings with the app's ID and icon. Closing the window is left to
/// the app (see `RssApp`): with the tray, it closes the window but keeps
/// running.
pub fn window_settings(size: iced::Size, min_size: Option<iced::Size>) -> iced::window::Settings {
    iced::window::Settings {
        size,
        min_size,
        icon: icon_rgba(128).and_then(|(rgba, size)| iced::window::icon::from_rgba(rgba, size, size).ok()),
        exit_on_close_request: false,
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: APP_ID.to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The main window's settings
pub fn main_window_settings() -> iced::window::Settings {
    window_settings(iced::Size::new(1200.0, 750.0), Some(iced::Size::new(900.0, 550.0)))
}

/// The icon drawn at `size` pixels square, as straight (not premultiplied)
/// RGBA. For the window (used on X11, and when the launcher isn't
/// installed) and the tray.
fn icon_rgba(size: u32) -> Option<(Vec<u8>, u32)> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(ICON_SVG, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Some((rgba, size))
}

/// What the tray or a second copy of the app asks the running app to do
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Show the window (opening it if it's closed)
    Show,
    Refresh,
    Quit,
}

fn channel() -> &'static (UnboundedSender<Command>, Mutex<Option<UnboundedReceiver<Command>>>) {
    static CHANNEL: OnceLock<(UnboundedSender<Command>, Mutex<Option<UnboundedReceiver<Command>>>)> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (sender, receiver) = unbounded_channel();
        (sender, Mutex::new(Some(receiver)))
    })
}

fn send(command: Command) {
    let _ = channel().0.send(command);
}

/// The commands, for a subscription. (Only one subscription can take them.)
pub fn commands() -> impl Stream<Item = Command> {
    let receiver = channel().1.lock().ok().and_then(|mut slot| slot.take());
    stream::unfold(receiver, |receiver| async move {
        let mut receiver = receiver?;
        let command = receiver.recv().await?;
        Some((command, Some(receiver)))
    })
}

// --- The tray icon ---

struct Tray;

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        APP_ID.to_string()
    }

    fn title(&self) -> String {
        "Shreader".to_string()
    }

    fn icon_name(&self) -> String {
        // The installed icon, if the launcher is installed…
        APP_ID.to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // …and otherwise this. Tray pixmaps are ARGB, big-endian.
        [32, 64]
            .into_iter()
            .filter_map(icon_rgba)
            .map(|(rgba, size)| ksni::Icon {
                width: size as i32,
                height: size as i32,
                data: rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect(),
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Shreader".to_string(), ..Default::default() }
    }

    /// A click on the icon
    fn activate(&mut self, _x: i32, _y: i32) {
        send(Command::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let item = |label: &str, command: Command| {
            StandardItem {
                label: label.to_string(),
                activate: Box::new(move |_: &mut Self| send(command)),
                ..Default::default()
            }
            .into()
        };
        vec![
            item("Show Shreader", Command::Show),
            item("Refresh All Feeds", Command::Refresh),
            ksni::MenuItem::Separator,
            item("Quit", Command::Quit),
        ]
    }
}

/// Shows the tray icon. Returns whether the desktop has a tray to show it
/// in (GNOME, for one, needs an extension for that); without one, closing
/// the window has to quit the app, or it couldn't be brought back.
pub async fn start_tray() -> bool {
    use ksni::TrayMethods;
    match Tray.spawn().await {
        Ok(handle) => {
            // Runs as long as the handle exists: keep it for the app's life
            static HANDLE: OnceLock<ksni::Handle<Tray>> = OnceLock::new();
            let _ = HANDLE.set(handle);
            true
        }
        Err(e) => {
            tracing::info!("No system tray: {}", e);
            false
        }
    }
}

// --- One copy at a time ---

/// Lets a second copy of the app ask this one to show its window.
struct Activator;

#[zbus::interface(name = "io.github.bmharks.Shreader")]
impl Activator {
    fn activate(&self) {
        send(Command::Show);
    }
}

/// Whether this is the only copy of the app.
pub enum Instance {
    /// It is: carry on
    First,
    /// Another copy is running, and has been asked to show its window
    AlreadyRunning,
}

/// Takes the app's D-Bus name, or if another copy has it, asks that copy to
/// show its window. Without a D-Bus session (rare), just runs.
pub async fn claim_single_instance() -> Instance {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    let Ok(connection) = zbus::Connection::session().await else {
        return Instance::First;
    };
    match connection.request_name_with_flags(APP_ID, RequestNameFlags::DoNotQueue.into()).await {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            if let Err(e) = connection.object_server().at(DBUS_PATH, Activator).await {
                tracing::warn!("Couldn't offer activation over D-Bus: {}", e);
            }
            // The name is held as long as the connection is open
            static CONNECTION: OnceLock<zbus::Connection> = OnceLock::new();
            let _ = CONNECTION.set(connection);
            Instance::First
        }
        Ok(_) | Err(zbus::Error::NameTaken) => {
            let asked = connection.call_method(Some(APP_ID), DBUS_PATH, Some(APP_ID), "Activate", &()).await;
            if let Err(e) = asked {
                // The other copy didn't answer (an older version without
                // this?): better a second window than none
                tracing::warn!("Another copy is running but didn't respond: {}", e);
                return Instance::First;
            }
            Instance::AlreadyRunning
        }
        Err(e) => {
            tracing::warn!("Couldn't check for another running copy: {}", e);
            Instance::First
        }
    }
}
