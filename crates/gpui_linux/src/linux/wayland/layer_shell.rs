pub use gpui::layer_shell::*;

use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

impl From<Layer> for zwlr_layer_shell_v1::Layer {
    fn from(layer: Layer) -> Self {
        match layer {
            Layer::Background => Self::Background,
            Layer::Bottom => Self::Bottom,
            Layer::Top => Self::Top,
            Layer::Overlay => Self::Overlay,
        }
    }
}

impl From<Anchor> for zwlr_layer_surface_v1::Anchor {
    fn from(anchor: Anchor) -> Self {
        Self::from_bits_truncate(anchor.bits())
    }
}

impl From<KeyboardInteractivity> for zwlr_layer_surface_v1::KeyboardInteractivity {
    fn from(value: KeyboardInteractivity) -> Self {
        match value {
            KeyboardInteractivity::None => Self::None,
            KeyboardInteractivity::Exclusive => Self::Exclusive,
            KeyboardInteractivity::OnDemand => Self::OnDemand,
        }
    }
}
