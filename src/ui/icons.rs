//! Embedded stroke icons, served to GPUI's `svg()` element as assets.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Pause,
    SkipBack,
    SkipForward,
    Repeat,
    RepeatOne,
    Volume,
    VolumeMuted,
    Plus,
    Close,
    Music,
    Film,
    PanelLeft,
    PanelRight,
    Info,
    Search,
    ChevronRight,
    ChevronDown,
    Folder,
    Display,
    Settings,
    Check,
    Minus,
}

impl Icon {
    pub fn path(self) -> SharedString {
        format!("icons/{}.svg", self.name()).into()
    }

    fn name(self) -> &'static str {
        match self {
            Icon::Play => "play",
            Icon::Pause => "pause",
            Icon::SkipBack => "skip-back",
            Icon::SkipForward => "skip-forward",
            Icon::Repeat => "repeat",
            Icon::RepeatOne => "repeat-one",
            Icon::Volume => "volume",
            Icon::VolumeMuted => "volume-muted",
            Icon::Plus => "plus",
            Icon::Close => "close",
            Icon::Music => "music",
            Icon::Film => "film",
            Icon::PanelLeft => "panel-left",
            Icon::PanelRight => "panel-right",
            Icon::Info => "info",
            Icon::Search => "search",
            Icon::ChevronRight => "chevron-right",
            Icon::ChevronDown => "chevron-down",
            Icon::Folder => "folder",
            Icon::Display => "display",
            Icon::Settings => "settings",
            Icon::Check => "check",
            Icon::Minus => "minus",
        }
    }

    fn body(self) -> &'static str {
        match self {
            Icon::Play => r##"<path d="M7 4.5v15l12-7.5z" fill="#fff" stroke-linejoin="round"/>"##,
            Icon::Pause => r##"<rect x="6" y="4.5" width="4" height="15" rx="1" fill="#fff"/><rect x="14" y="4.5" width="4" height="15" rx="1" fill="#fff"/>"##,
            Icon::SkipBack => r##"<path d="M18 5v14L8 12z" fill="#fff"/><path d="M6 5v14"/>"##,
            Icon::SkipForward => r##"<path d="M6 5v14l10-7z" fill="#fff"/><path d="M18 5v14"/>"##,
            Icon::Repeat => r##"<path d="M17 2l4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="M7 22l-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/>"##,
            Icon::RepeatOne => r##"<path d="M17 2l4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="M7 22l-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/><path d="M11 10l1.5-1v6"/>"##,
            Icon::Volume => r##"<path d="M11 5L6 9H2v6h4l5 4z" fill="#fff"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a10 10 0 0 1 0 14"/>"##,
            Icon::VolumeMuted => r##"<path d="M11 5L6 9H2v6h4l5 4z" fill="#fff"/><path d="M22 9l-6 6"/><path d="M16 9l6 6"/>"##,
            Icon::Plus => r##"<path d="M12 5v14M5 12h14"/>"##,
            Icon::Close => r##"<path d="M6 6l12 12M18 6L6 18"/>"##,
            Icon::Music => r##"<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>"##,
            Icon::Film => r##"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M7 3v18M17 3v18M3 12h18M3 7.5h4M3 16.5h4M17 7.5h4M17 16.5h4"/>"##,
            Icon::PanelLeft => r##"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M9 3v18"/>"##,
            Icon::PanelRight => r##"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M15 3v18"/>"##,
            Icon::Info => r##"<circle cx="12" cy="12" r="9"/><path d="M12 16v-5M12 8h.01"/>"##,
            Icon::Search => r##"<circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/>"##,
            Icon::ChevronRight => r##"<path d="M9 6l6 6-6 6"/>"##,
            Icon::ChevronDown => r##"<path d="M6 9l6 6 6-6"/>"##,
            Icon::Folder => r##"<path d="M3 6.5A1.5 1.5 0 0 1 4.5 5h4.6l2 2h8.4A1.5 1.5 0 0 1 21 8.5v9a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 17.5z"/>"##,
            Icon::Display => r##"<rect x="2.5" y="4" width="19" height="13" rx="2"/><path d="M8 21h8M12 17v4"/>"##,
            Icon::Check => r##"<path d="M5 12.5l4.5 4.5L19 7"/>"##,
            Icon::Minus => r##"<path d="M5 12h14"/>"##,
            Icon::Settings => r##"<circle cx="12" cy="12" r="3"/><circle cx="12" cy="12" r="6.5"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1"/>"##,
        }
    }

    const ALL: [Icon; 23] = [
        Icon::Play,
        Icon::Pause,
        Icon::SkipBack,
        Icon::SkipForward,
        Icon::Repeat,
        Icon::RepeatOne,
        Icon::Volume,
        Icon::VolumeMuted,
        Icon::Plus,
        Icon::Close,
        Icon::Music,
        Icon::Film,
        Icon::PanelLeft,
        Icon::PanelRight,
        Icon::Info,
        Icon::Search,
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::Folder,
        Icon::Display,
        Icon::Settings,
        Icon::Check,
        Icon::Minus,
    ];
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(icon) = Icon::ALL.iter().find(|i| i.path().as_ref() == path) else {
            return Ok(None);
        };
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"##,
            icon.body()
        );
        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL.iter().map(|i| i.path()).filter(|p| p.starts_with(path)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_with_the_renderer_gpui_uses() {
        for icon in Icon::ALL {
            let data = Assets.load(&icon.path()).unwrap().expect("icon served");
            let tree = usvg::Tree::from_data(&data, &usvg::Options::default());
            assert!(tree.is_ok(), "{icon:?}: {:?}", tree.err());
            assert!(tree.unwrap().root().has_children(), "{icon:?} is empty");
        }
        assert!(Assets.load("icons/missing.svg").unwrap().is_none());
    }
}
