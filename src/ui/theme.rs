//! Zed's default "One Dark" palette.

use gpui::{Hsla, rgb, rgba};

pub struct Theme;

#[allow(dead_code)]
impl Theme {
    pub fn editor_bg() -> Hsla {
        rgb(0x282c33).into()
    }
    pub fn panel_bg() -> Hsla {
        rgb(0x2f343e).into()
    }
    pub fn title_bar_bg() -> Hsla {
        rgb(0x3b414d).into()
    }
    pub fn status_bar_bg() -> Hsla {
        rgb(0x3b414d).into()
    }
    pub fn tab_bar_bg() -> Hsla {
        rgb(0x2f343e).into()
    }
    pub fn tab_active_bg() -> Hsla {
        rgb(0x282c33).into()
    }
    pub fn elevated_bg() -> Hsla {
        rgb(0x2f343e).into()
    }
    pub fn border() -> Hsla {
        rgb(0x464b57).into()
    }
    pub fn border_variant() -> Hsla {
        rgb(0x363c46).into()
    }
    pub fn border_focused() -> Hsla {
        rgb(0x47679e).into()
    }
    pub fn text() -> Hsla {
        rgb(0xdce0e5).into()
    }
    pub fn text_muted() -> Hsla {
        rgb(0xa9afbc).into()
    }
    pub fn text_placeholder() -> Hsla {
        rgb(0x878a98).into()
    }
    pub fn accent() -> Hsla {
        rgb(0x74ade8).into()
    }
    pub fn hover() -> Hsla {
        rgb(0x363c46).into()
    }
    pub fn selected() -> Hsla {
        rgb(0x454a56).into()
    }
    pub fn track() -> Hsla {
        rgb(0x464b57).into()
    }
    pub fn error() -> Hsla {
        rgb(0xd07277).into()
    }
    pub fn success() -> Hsla {
        rgb(0xa1c181).into()
    }
    pub fn warning() -> Hsla {
        rgb(0xdec184).into()
    }
    pub fn shadow() -> Hsla {
        rgba(0x0000004d).into()
    }
}

/// Monospace face for timecodes, per platform.
pub fn mono_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}
