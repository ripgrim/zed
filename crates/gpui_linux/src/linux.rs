mod dispatcher;
mod headless;
mod keyboard;
mod platform;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod text_system;
#[cfg(feature = "wayland")]
mod wayland;
#[cfg(feature = "x11")]
mod x11;

#[cfg(any(feature = "wayland", feature = "x11"))]
mod xdg_desktop_portal;

pub use dispatcher::*;
pub use headless::*;
pub use keyboard::*;
pub use platform::*;
#[cfg(any(feature = "wayland", feature = "x11"))]
pub use text_system::*;
#[cfg(feature = "wayland")]
pub use wayland::*;
#[cfg(feature = "x11")]
pub use x11::*;

#[cfg(all(feature = "screen-capture", any(feature = "wayland", feature = "x11")))]
pub type PlatformScreenCaptureFrame = scap::frame::Frame;
#[cfg(not(all(feature = "screen-capture", any(feature = "wayland", feature = "x11"))))]
pub type PlatformScreenCaptureFrame = ();

use std::rc::Rc;

/// Returns the default platform implementation for the current OS.
pub fn current_platform(headless: bool) -> Rc<dyn gpui::Platform> {
    #[cfg(feature = "x11")]
    use anyhow::Context as _;

    if headless {
        return Rc::new(HeadlessClient::new());
    }

    match gpui::guess_compositor() {
        #[cfg(feature = "wayland")]
        "Wayland" => Rc::new(WaylandClient::new()),

        #[cfg(feature = "x11")]
        "X11" => Rc::new(
            X11Client::new()
                .context("Failed to initialize X11 client.")
                .unwrap(),
        ),

        "Headless" => Rc::new(HeadlessClient::new()),
        _ => unreachable!(),
    }
}
