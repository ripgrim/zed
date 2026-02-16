//! Convenience crate that re-exports GPUI's platform traits and the
//! `current_platform` constructor so consumers don't need `#[cfg]` gating.

pub use gpui::{Platform, current_platform};
use std::rc::Rc;

/// Returns the default [`Platform`] for the current OS.
///
/// This is a thin wrapper around [`gpui::current_platform`].
pub fn platform(headless: bool) -> Rc<dyn Platform> {
    current_platform(headless)
}
