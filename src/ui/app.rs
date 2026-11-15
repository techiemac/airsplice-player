//! The main window: a Zed-style workspace with a playlist dock, a "buffer"
//! area for the video or now-playing view, an info dock, a status bar, and a
//! command palette and file finder.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    Action, AnyElement, App, Bounds, ClickEvent, Context, DisplayId, ExternalPaths, FocusHandle, FontWeight, HighlightStyle,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, PathPromptOptions, Pixels,
    RenderImage, ScrollWheelEvent, SharedString, StyledText, Task, Window, WindowHandle, actions, canvas, div, img, px, svg,
};

use super::folders::{self, Folders, Scan, Stamp};
use super::fuzzy::fuzzy_match;
use super::icons::Icon;
use super::os_media::{NowPlaying, OsMedia, PlayState};
use super::output::{self, OutputView};
use super::session::{Session, SessionStore, TabState};
use super::theme::{Theme, mono_font};
use crate::media::upscale::{self, Status as UpscaleStatus};
use crate::media::picture::is_picture;
use crate::media::{Image, Player, Summary, probe};

mod settings;
use settings::{Output, SETTINGS_CONTEXT, SettingsScreen};

actions!(
    airsplice_player,
    [
        PlayPause,
        NextTrack,
        PreviousTrack,
        SeekForward,
        SeekBackward,
        VolumeUp,
        VolumeDown,
        ToggleMute,
        CycleRepeat,
        OpenFiles,
        ToggleSidebar,
        ToggleInfoPanel,
        ToggleFullscreen,
        CommandPalette,
        FileFinder,
        ClearPlaylist,
        Stop,
        Quit,
        CloseTab,
        NextTab,
        PreviousTab,
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateLastTab,
        PickerUp,
        PickerDown,
        PickerConfirm,
        PickerConfirmInNewTab,
        PickerDismiss,
        ToggleOutputs,
        OpenSettings,
        SettingsLess,
        SettingsMore,
        ToggleAutoplay,
    ]
);

const PLAYER_CONTEXT: &str = "Player";
const PICKER_CONTEXT: &str = "Picker";
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;
/// Each open tab keeps a decoder, an audio stream and a few frames alive.
const MAX_TABS: usize = 8;

pub fn bind_keys(cx: &mut App) {
    let player = Some(PLAYER_CONTEXT);
    let picker = Some(PICKER_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("space", PlayPause, player),
        KeyBinding::new("k", PlayPause, player),
        KeyBinding::new("right", SeekForward, player),
        KeyBinding::new("l", SeekForward, player),
        KeyBinding::new("left", SeekBackward, player),
        KeyBinding::new("j", SeekBackward, player),
        KeyBinding::new("up", VolumeUp, player),
        KeyBinding::new("down", VolumeDown, player),
        KeyBinding::new("m", ToggleMute, player),
        KeyBinding::new("r", CycleRepeat, player),
        KeyBinding::new("f", ToggleFullscreen, player),
        KeyBinding::new("n", NextTrack, player),
        KeyBinding::new("p", PreviousTrack, player),
        KeyBinding::new("secondary-right", NextTrack, None),
        KeyBinding::new("secondary-left", PreviousTrack, None),
        KeyBinding::new("secondary-o", OpenFiles, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-r", ToggleInfoPanel, None),
        KeyBinding::new("secondary-shift-p", CommandPalette, None),
        KeyBinding::new("secondary-p", FileFinder, None),
        KeyBinding::new("secondary-.", Stop, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseTab, None),
        KeyBinding::new("secondary-shift-]", NextTab, None),
        KeyBinding::new("secondary-shift-[", PreviousTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        // As in browsers and editors: 1 to 8 pick that tab, 9 the last one.
        KeyBinding::new("secondary-1", ActivateTab1, None),
        KeyBinding::new("secondary-2", ActivateTab2, None),
        KeyBinding::new("secondary-3", ActivateTab3, None),
        KeyBinding::new("secondary-4", ActivateTab4, None),
        KeyBinding::new("secondary-5", ActivateTab5, None),
        KeyBinding::new("secondary-6", ActivateTab6, None),
        KeyBinding::new("secondary-7", ActivateTab7, None),
        KeyBinding::new("secondary-8", ActivateTab8, None),
        KeyBinding::new("secondary-9", ActivateLastTab, None),
        KeyBinding::new("up", PickerUp, picker),
        KeyBinding::new("down", PickerDown, picker),
        KeyBinding::new("enter", PickerConfirm, picker),
        KeyBinding::new("secondary-enter", PickerConfirmInNewTab, picker),
        KeyBinding::new("escape", PickerDismiss, picker),
        // F is full screen here; with Shift, the outputs on other displays.
        KeyBinding::new("secondary-shift-f", ToggleOutputs, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("up", PickerUp, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("down", PickerDown, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("enter", PickerConfirm, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("escape", PickerDismiss, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("left", SettingsLess, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("right", SettingsMore, Some(SETTINGS_CONTEXT)),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repeat {
    Off,
    All,
    One,
}

impl Repeat {
    fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
    fn key(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
    fn from_key(key: &str) -> Self {
        match key {
            "all" => Repeat::All,
            "one" => Repeat::One,
            _ => Repeat::Off,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Repeat::Off => "Repeat off",
            Repeat::All => "Repeat all",
            Repeat::One => "Repeat one",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Seek,
    Volume,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Commands,
    Files,
}

struct Picker {
    kind: PickerKind,
    query: String,
    selected: usize,
}

struct PickerItem {
    label: SharedString,
    positions: Vec<usize>,
    detail: Option<SharedString>,
    target: PickerTarget,
}

/// One open file, ready to play: tabs keep several cued at once so the next
/// one starts without loading.
struct Tab {
    path: PathBuf,
    /// Its playlist entry, while that entry exists. For the active tab,
    /// `MediaPlayer::current` is authoritative.
    track: Option<usize>,
    /// Playback state of every tab except the active one, whose state lives
    /// in the `MediaPlayer` fields the rest of the UI reads.
    parked: Option<Loaded>,
}

/// A file's playback state, whether on screen or parked in a tab.
#[derive(Default)]
struct Loaded {
    player: Option<Player>,
    frame: Option<Arc<RenderImage>>,
    cover: Option<Arc<RenderImage>>,
    error: Option<String>,
    ended: bool,
}

impl Loaded {
    fn release(self, window: &mut Window) {
        for image in [self.frame, self.cover].into_iter().flatten() {
            let _ = window.drop_image(image);
        }
    }
}

enum PickerTarget {
    Action(Box<dyn Action>),
    Track(usize),
}

fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Play / Pause", Box::new(PlayPause)),
        ("Next Track", Box::new(NextTrack)),
        ("Previous Track", Box::new(PreviousTrack)),
        ("Seek Forward 5 Seconds", Box::new(SeekForward)),
        ("Seek Backward 5 Seconds", Box::new(SeekBackward)),
        ("Volume Up", Box::new(VolumeUp)),
        ("Volume Down", Box::new(VolumeDown)),
        ("Toggle Mute", Box::new(ToggleMute)),
        ("Cycle Repeat Mode", Box::new(CycleRepeat)),
        ("Toggle Autoplay", Box::new(ToggleAutoplay)),
        ("Open Files…", Box::new(OpenFiles)),
        ("Toggle Playlist Panel", Box::new(ToggleSidebar)),
        ("Toggle Info Panel", Box::new(ToggleInfoPanel)),
        ("Toggle Full Screen", Box::new(ToggleFullscreen)),
        ("Toggle Outputs", Box::new(ToggleOutputs)),
        ("Settings", Box::new(OpenSettings)),
        ("Find in Playlist", Box::new(FileFinder)),
        ("Stop", Box::new(Stop)),
        ("Close Tab", Box::new(CloseTab)),
        ("Next Tab", Box::new(NextTab)),
        ("Previous Tab", Box::new(PreviousTab)),
        ("Clear Playlist", Box::new(ClearPlaylist)),
        ("Quit", Box::new(Quit)),
    ]
}

pub struct MediaPlayer {
    focus: FocusHandle,
    picker_focus: FocusHandle,
    playlist: Vec<PathBuf>,
    /// Open tabs, left to right; `active` is `None` exactly when there are
    /// none. The active tab's state is in `current`, `player`, `frame`...
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// A short message about the last command (e.g. the tab limit).
    notice: Option<String>,
    /// The output windows open now, one per display.
    outputs: Vec<Output>,
    /// Stable ids of the displays chosen as outputs, connected or not.
    output_setting: Vec<String>,
    /// The outputs' on/off switch (the status bar button). Off keeps the
    /// chosen displays for next time.
    outputs_on: bool,
    /// The largest output window, in physical pixels; video is decoded
    /// and upscaled for whichever is bigger, this or the main window.
    outputs_px: (u32, u32),
    /// Upscale video on the GPU to the size it is shown at (a setting).
    upscaling: bool,
    /// Seconds a still shows before the playlist moves on; 0: until
    /// skipped (a setting).
    still_seconds: u32,
    /// Move on to the next playlist entry when one ends (the playlist
    /// pane's switch). Off, playback stops at the end of each entry.
    autoplay: bool,
    /// The entry the playlist will play next, already open in the
    /// background and cued, so moving on is instant.
    preloaded: Option<(PathBuf, Loaded)>,
    /// When the entry on screen was opened: preloading waits a moment so
    /// it does not compete with that start.
    opened_at: Instant,
    /// The display size the paused frame was last refreshed for, and when
    /// the size last changed: a paused frame is decoded again once the
    /// size settles, so upscaling follows resizes and the switch.
    limit: (u32, u32),
    limit_changed: Instant,
    refreshed_for: Option<((u32, u32), bool)>,
    /// Output windows asked for but not open yet.
    outputs_opening: Vec<String>,
    output_checked: Instant,
    settings: Option<SettingsScreen>,
    settings_focus: FocusHandle,
    current: Option<usize>,
    player: Option<Player>,
    ended: bool,
    frame: Option<Arc<RenderImage>>,
    cover: Option<Arc<RenderImage>>,
    error: Option<String>,
    volume: f32,
    muted: bool,
    repeat: Repeat,
    sidebar_open: bool,
    info_open: bool,
    picker: Option<Picker>,
    seek_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    volume_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Option<Drag>,
    drag_fraction: f64,
    last_drag_seek: Instant,
    drawn_position: f64,
    /// Frames shown since `fps_since`, for the `RUST_LOG=airsplice_player=debug`
    /// presentation-rate log.
    fps_count: u32,
    fps_since: Instant,
    /// Tags and durations for playlist entries, filled in the background.
    summaries: std::collections::HashMap<PathBuf, Summary>,
    /// Folders added to the playlist, watched for files added later.
    folders: Folders,
    /// Files in those folders the user removed; rescans leave them out.
    removed_from_folders: std::collections::BTreeSet<PathBuf>,
    /// Size and modification time of folder files when last seen, so a file
    /// that changes (a copy finishing) has its tags read again.
    stamps: std::collections::HashMap<PathBuf, Stamp>,
    os_media: Option<OsMedia>,
    store: Option<SessionStore>,
    saved: Option<Session>,
    saved_at: Instant,
    _ticker: Task<()>,
    _on_quit: gpui::Subscription,
}

impl MediaPlayer {
    /// Files given on the command line replace the saved playlist; otherwise
    /// the last session is restored, paused where it was left.
    pub fn new(files: Vec<PathBuf>, store: Option<SessionStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session = store.as_ref().map(SessionStore::load).unwrap_or_default();
        let focus = cx.focus_handle();
        window.focus(&focus);
        let ticker = cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(delay) = this.update(cx, |this, _| this.tick_interval()) else { break };
                cx.background_executor().timer(delay).await;
                if this.update_in(cx, |this, window, cx| this.tick(window, cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            focus,
            picker_focus: cx.focus_handle(),
            playlist: Vec::new(),
            tabs: Vec::new(),
            active: None,
            notice: None,
            outputs: Vec::new(),
            output_setting: session.output_displays.clone(),
            outputs_opening: Vec::new(),
            outputs_on: session.outputs_on,
            outputs_px: (0, 0),
            upscaling: session.upscaling,
            still_seconds: session.still_seconds,
            autoplay: session.autoplay,
            preloaded: None,
            opened_at: Instant::now(),
            limit: (0, 0),
            limit_changed: Instant::now(),
            refreshed_for: None,
            // Long ago, so the first tick opens a remembered output.
            output_checked: Instant::now().checked_sub(Duration::from_secs(60)).unwrap_or_else(Instant::now),
            settings: None,
            settings_focus: cx.focus_handle(),
            current: None,
            player: None,
            ended: false,
            frame: None,
            cover: None,
            error: None,
            volume: session.volume,
            muted: session.muted,
            repeat: Repeat::from_key(&session.repeat),
            sidebar_open: session.sidebar_open,
            info_open: session.info_open,
            picker: None,
            seek_bounds: Rc::default(),
            volume_bounds: Rc::default(),
            drag: None,
            drag_fraction: 0.0,
            last_drag_seek: Instant::now(),
            drawn_position: -1.0,
            fps_count: 0,
            fps_since: Instant::now(),
            summaries: Default::default(),
            folders: Folders::default(),
            removed_from_folders: Default::default(),
            stamps: Default::default(),
            os_media: None,
            store,
            saved: None,
            saved_at: Instant::now(),
            _ticker: ticker,
            _on_quit: cx.on_app_quit(|this, _| {
                this.save_session(true);
                async {}
            }),
        };
        if files.is_empty() {
            this.playlist = session.playlist;
            this.removed_from_folders = session.removed_from_folders.into_iter().collect();
            // Rescans now, for files added while the app was closed.
            this.folders.set(session.folders);
            this.probe_new(cx);
            if !session.tabs.is_empty() {
                this.restore_tabs(&session.tabs, session.active_tab, window, cx);
            } else if let Some(ix) = session.current {
                this.open_index(ix, false, session.position, window, cx);
            }
            this.saved = Some(this.snapshot(false));
        } else {
            this.add_paths(&files, window, cx);
        }
        this
    }

    /// Hook up hardware media keys and the system "Now Playing" view.
    /// Called by the app, not tests, so tests never touch OS state.
    pub fn enable_os_media(&mut self, window: &Window) {
        #[cfg(target_os = "windows")]
        let hwnd = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            // GPUI's own `Window::window_handle` would shadow the trait's.
            match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get() as *mut std::ffi::c_void),
                _ => None,
            }
        };
        #[cfg(not(target_os = "windows"))]
        let hwnd = {
            let _ = window;
            None
        };
        self.os_media = OsMedia::new(hwnd);
    }

    fn now_playing(&self) -> Option<NowPlaying> {
        let p = self.player.as_ref()?;
        Some(NowPlaying {
            title: p.info.title.clone(),
            artist: p.info.artist.clone(),
            album: p.info.album.clone(),
            duration: p.info.duration,
            state: if self.ended {
                PlayState::Stopped
            } else if p.is_paused() {
                PlayState::Paused
            } else {
                PlayState::Playing
            },
            position: p.position(),
        })
    }

    /// Apply media-key events and refresh the OS view.
    fn sync_os_media(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(os) = &self.os_media else { return };
        use souvlaki::{MediaControlEvent as E, SeekDirection};
        for event in os.poll() {
            let paused = self.player.as_ref().is_none_or(|p| p.is_paused() || self.ended);
            match event {
                E::Toggle => self.play_pause(&PlayPause, window, cx),
                E::Play if paused => self.play_pause(&PlayPause, window, cx),
                E::Pause if !paused => self.play_pause(&PlayPause, window, cx),
                E::Next => self.next_track(&NextTrack, window, cx),
                E::Previous => self.previous_track(&PreviousTrack, window, cx),
                E::Stop => self.stop(&Stop, window, cx),
                E::Seek(SeekDirection::Forward) => self.seek_forward(&SeekForward, window, cx),
                E::Seek(SeekDirection::Backward) => self.seek_backward(&SeekBackward, window, cx),
                E::SeekBy(dir, by) => {
                    if let Some(pos) = self.player.as_ref().map(Player::position) {
                        let delta = by.as_secs_f64() * if dir == SeekDirection::Forward { 1.0 } else { -1.0 };
                        self.seek_to(pos + delta, cx);
                    }
                }
                E::SetPosition(p) => self.seek_to(p.0.as_secs_f64(), cx),
                E::SetVolume(v) => self.set_volume(v as f32, cx),
                E::Raise => window.activate_window(),
                E::Quit => cx.quit(),
                _ => {}
            }
        }
        let now = self.now_playing();
        if let Some(os) = self.os_media.as_mut() {
            os.update(now);
        }
    }

    // ---- session ----------------------------------------------------------

    fn snapshot(&self, exact: bool) -> Session {
        let saved_position = |player: Option<&Player>, ended: bool| match player {
            Some(_) if ended => 0.0,
            Some(p) if exact => p.position(),
            // Whole seconds, so steady playback saves every few seconds
            // rather than on every tick.
            Some(p) => p.position().floor(),
            None => 0.0,
        };
        let position = saved_position(self.player.as_ref(), self.ended);
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| TabState {
                path: tab.path.clone(),
                position: if self.active == Some(ix) {
                    position
                } else {
                    tab.parked.as_ref().map_or(0.0, |l| saved_position(l.player.as_ref(), l.ended))
                },
            })
            .collect();
        Session {
            playlist: self.playlist.clone(),
            current: self.current,
            position,
            volume: self.volume,
            muted: self.muted,
            repeat: self.repeat.key().into(),
            sidebar_open: self.sidebar_open,
            info_open: self.info_open,
            tabs,
            active_tab: self.active,
            folders: self.folders.roots().to_vec(),
            removed_from_folders: self.removed_from_folders.iter().cloned().collect(),
            output_displays: self.output_setting.clone(),
            outputs_on: self.outputs_on,
            upscaling: self.upscaling,
            still_seconds: self.still_seconds,
            autoplay: self.autoplay,
        }
    }

    /// Save when anything changed, at most every 2 s unless `force`.
    fn save_session(&mut self, force: bool) {
        let Some(store) = &self.store else { return };
        if !force && self.saved_at.elapsed() < Duration::from_secs(2) {
            return;
        }
        let snapshot = self.snapshot(force);
        if self.saved.as_ref() == Some(&snapshot) {
            return;
        }
        if let Err(e) = store.save(&snapshot) {
            log::warn!("saving session: {e:#}");
        }
        self.saved = Some(snapshot);
        self.saved_at = Instant::now();
    }

    // ---- playback ---------------------------------------------------------

    fn tick_interval(&self) -> Duration {
        match &self.player {
            Some(p) if !p.is_paused() && p.info.video.is_some() => Duration::from_millis(5),
            Some(p) if !p.is_paused() => Duration::from_millis(33),
            // Paused: still poll briefly so a paused seek can show its frame.
            Some(_) => Duration::from_millis(16),
            None => Duration::from_millis(100),
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(scan) = self.folders.poll() {
            self.apply_scan(scan, window, cx);
        }
        self.poll_output(window, cx);
        self.save_session(false);
        self.sync_os_media(window, cx);
        // Background tabs are paused; this picks up the frame each one is
        // cued on, so switching to it shows a picture at once.
        self.preload_next(window);
        let preloaded = self.preloaded.as_mut().map(|(_, l)| l);
        for loaded in self.tabs.iter_mut().filter_map(|t| t.parked.as_mut()).chain(preloaded) {
            if let Some(frame) = loaded.player.as_ref().and_then(Player::take_frame)
                && let Some(old) = loaded.frame.replace(to_render_image(frame.width, frame.height, frame.bgra))
            {
                let _ = window.drop_image(old);
            }
        }
        if self.player.is_none() {
            return;
        }
        // Video is never shown larger than the biggest window showing it:
        // decoders that convert on the CPU skip pixels nobody would see,
        // and upscaling stops there.
        let size = window.viewport_size();
        let scale = window.scale_factor();
        let main = ((f32::from(size.width) * scale) as u32, (f32::from(size.height) * scale) as u32);
        let limit = if self.outputs_px.0 as u64 * self.outputs_px.1 as u64 > main.0 as u64 * main.1 as u64 { self.outputs_px } else { main };
        if limit != self.limit {
            self.limit = limit;
            self.limit_changed = Instant::now();
        }
        self.refresh_paused_frame();
        let Some(player) = &self.player else { return };
        player.set_output_limit(limit.0, limit.1);
        let mut changed = false;
        if let Some(frame) = player.take_frame() {
            let image = to_render_image(frame.width, frame.height, frame.bgra);
            if let Some(old) = self.frame.replace(image) {
                let _ = window.drop_image(old);
            }
            changed = true;
            self.fps_count += 1;
            let elapsed = self.fps_since.elapsed();
            if elapsed >= Duration::from_secs(2) {
                log::debug!("presented {:.1} frames/s", self.fps_count as f64 / elapsed.as_secs_f64());
                self.fps_count = 0;
                self.fps_since = Instant::now();
            }
        }
        let position = player.position();
        if (position - self.drawn_position).abs() >= 0.04 || (!player.is_paused() && player.info.audio.is_some()) {
            changed = true;
        }
        if let Some(e) = player.error() {
            self.error = Some(e);
        }
        if !self.ended && player.is_finished() {
            self.on_track_end(window, cx);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// The playlist entry that follows the one on screen when it ends, by
    /// the autoplay switch and repeat mode; `None` if playback stops there
    /// (or repeats the same entry).
    fn upcoming(&self) -> Option<usize> {
        let ix = self.current?;
        let len = self.playlist.len();
        if !self.autoplay || self.repeat == Repeat::One {
            return None;
        }
        match self.repeat {
            Repeat::All if len > 1 => Some((ix + 1) % len),
            Repeat::Off if ix + 1 < len => Some(ix + 1),
            _ => None,
        }
    }

    /// Keep the upcoming entry open in the background, cued on its first
    /// frame, so the playlist moves on without a load (a large image takes
    /// a while to decode). Dropped when the upcoming entry changes.
    fn preload_next(&mut self, window: &mut Window) {
        let want = self.upcoming().and_then(|ix| self.playlist.get(ix).cloned());
        if self.preloaded.as_ref().map(|(p, _)| p) == want.as_ref() {
            return;
        }
        if want.is_some() && self.opened_at.elapsed() < Duration::from_millis(1500) {
            return;
        }
        if let Some((_, l)) = self.preloaded.take() {
            l.release(window);
        }
        if let Some(path) = want {
            let loaded = self.load_in_background(&path, 0.0);
            self.preloaded = Some((path, loaded));
        }
    }

    fn toggle_autoplay(&mut self, _: &ToggleAutoplay, _: &mut Window, cx: &mut Context<Self>) {
        self.autoplay = !self.autoplay;
        cx.notify();
    }

    fn on_track_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.playlist.len();
        // Repeat one loops the entry; with autoplay off (or a tab whose file
        // has left the playlist) playback stops at the end.
        let ix = self.current.filter(|_| self.repeat != Repeat::One && self.autoplay);
        let Some(ix) = ix else {
            match self.player.as_mut() {
                Some(p) if self.repeat == Repeat::One => p.seek(0.0),
                Some(p) => {
                    self.ended = true;
                    p.set_paused(true);
                }
                None => {}
            }
            return;
        };
        match self.repeat {
            Repeat::One => {}
            Repeat::All => self.play_index((ix + 1) % len, window, cx),
            Repeat::Off if ix + 1 < len => self.play_index(ix + 1, window, cx),
            Repeat::Off => {
                self.ended = true;
                if let Some(p) = self.player.as_mut() {
                    p.set_paused(true);
                }
            }
        }
    }

    fn play_index(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_index(ix, true, 0.0, window, cx);
    }

    /// Open playlist entry `ix` in the active tab (a new one if none is
    /// open), replacing what it showed.
    fn open_index(&mut self, ix: usize, autoplay: bool, start_at: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.playlist.get(ix).cloned() else { return };
        self.unload(window);
        let loaded = match self.preloaded.take() {
            // Already open and cued: take it over instead of opening again.
            Some((p, mut loaded)) if p == path && start_at == 0.0 => {
                if let Some(player) = loaded.player.as_mut() {
                    player.set_background(false);
                    player.set_volume(self.volume, self.muted);
                    player.set_paused(!autoplay);
                }
                loaded
            }
            other => {
                if let Some((_, l)) = other {
                    l.release(window);
                }
                self.load(&path, autoplay, start_at)
            }
        };
        self.install(loaded);
        self.opened_at = Instant::now();
        self.current = Some(ix);
        match self.active {
            Some(a) => self.tabs[a].path = path,
            None => {
                self.tabs.push(Tab { path, track: Some(ix), parked: None });
                self.active = Some(self.tabs.len() - 1);
            }
        }
        self.notice = None;
        self.update_title(window);
        cx.notify();
    }

    fn load(&self, path: &Path, autoplay: bool, start_at: f64) -> Loaded {
        match Player::open(path, self.volume, self.muted) {
            Ok(mut player) => {
                player.set_upscaling(self.upscaling);
                player.set_still_duration(self.still_duration());
                let cover = player.info.cover.as_ref().map(image_to_render);
                let error = player.info.video_unsupported.clone().map(|why| format!("Playing audio only: {why}"));
                if !autoplay {
                    player.set_paused(true);
                }
                if start_at > 0.0 {
                    player.seek(start_at);
                }
                Loaded { player: Some(player), cover, error, ..Default::default() }
            }
            Err(e) => Loaded { error: Some(format!("Can't play {}: {e:#}", file_name(path))), ..Default::default() },
        }
    }

    /// Load a file for a tab that is not on screen: paused, keeping only the
    /// frame it is cued on (and one more) decoded.
    fn load_in_background(&self, path: &Path, start_at: f64) -> Loaded {
        let mut loaded = self.load(path, false, start_at);
        if let Some(p) = loaded.player.as_mut() {
            p.set_background(true);
        }
        loaded
    }

    /// Make `loaded` the state on screen.
    fn install(&mut self, loaded: Loaded) {
        self.player = loaded.player;
        self.frame = loaded.frame;
        self.cover = loaded.cover;
        self.error = loaded.error;
        self.ended = loaded.ended;
        self.drawn_position = -1.0;
    }

    fn take_installed(&mut self) -> Loaded {
        Loaded {
            player: self.player.take(),
            frame: self.frame.take(),
            cover: self.cover.take(),
            error: self.error.take(),
            ended: std::mem::take(&mut self.ended),
        }
    }

    fn update_title(&self, window: &mut Window) {
        match &self.player {
            Some(p) => window.set_window_title(&format!("{} - airsplice-player", p.info.title)),
            None => window.set_window_title("airsplice-player"),
        }
    }

    // ---- tabs ---------------------------------------------------------------

    /// Open playlist entry `ix` in a new background tab, paused on its first
    /// frame, without leaving the current one.
    fn open_in_new_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.playlist.get(ix).cloned() else { return };
        if self.active.is_none() {
            self.open_index(ix, false, 0.0, window, cx);
            return;
        }
        if self.tabs.len() >= MAX_TABS {
            self.notice = Some(format!("{MAX_TABS} tabs are open; close one to open another"));
            cx.notify();
            return;
        }
        let loaded = self.load_in_background(&path, 0.0);
        self.tabs.push(Tab { path, track: Some(ix), parked: Some(loaded) });
        self.notice = None;
        cx.notify();
    }

    /// Show tab `ix`. If the tab being left was playing, the new one starts
    /// playing (a cut); the old one pauses where it was.
    fn activate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() || self.active == Some(ix) {
            return;
        }
        let mut was_playing = false;
        if let Some(a) = self.active {
            was_playing = !self.ended && self.player.as_ref().is_some_and(|p| !p.is_paused());
            if let Some(p) = self.player.as_mut() {
                p.set_paused(true);
                p.set_background(true);
            }
            self.tabs[a].track = self.current;
            self.tabs[a].parked = Some(self.take_installed());
        }
        let loaded = self.tabs[ix].parked.take().unwrap_or_default();
        self.install(loaded);
        self.current = self.tabs[ix].track;
        self.active = Some(ix);
        if let Some(p) = self.player.as_mut() {
            p.set_background(false);
            p.set_volume(self.volume, self.muted);
            if was_playing && !self.ended {
                p.set_paused(false);
            }
        }
        self.drag = None;
        self.notice = None;
        self.update_title(window);
        cx.notify();
    }

    /// Close tab `ix`. Closing the active tab shows its neighbour, paused.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        match self.active {
            Some(a) if a == ix => {
                self.unload(window);
                self.error = None;
                self.current = None;
                self.active = None;
                if self.tabs.is_empty() {
                    self.update_title(window);
                } else {
                    self.activate_tab(ix.min(self.tabs.len() - 1), window, cx);
                }
            }
            Some(a) => {
                if let Some(loaded) = tab.parked {
                    loaded.release(window);
                }
                if ix < a {
                    self.active = Some(a - 1);
                }
            }
            None => {}
        }
        self.notice = None;
        cx.notify();
    }

    fn close_all_tabs(&mut self, window: &mut Window) {
        self.unload(window);
        for loaded in self.tabs.drain(..).filter_map(|t| t.parked) {
            loaded.release(window);
        }
        self.active = None;
        self.current = None;
        self.error = None;
        self.update_title(window);
    }

    /// Reopen the tabs of a saved session, each paused where it was left.
    fn restore_tabs(&mut self, tabs: &[TabState], active: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        for tab in tabs.iter().take(MAX_TABS) {
            let loaded = self.load_in_background(&tab.path, tab.position);
            let track = self.playlist.iter().position(|p| *p == tab.path);
            self.tabs.push(Tab { path: tab.path.clone(), track, parked: Some(loaded) });
        }
        let active = active.filter(|a| *a < self.tabs.len()).or((!self.tabs.is_empty()).then_some(0));
        if let Some(a) = active {
            self.activate_tab(a, window, cx);
        }
    }

    fn close_active_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(a) = self.active {
            self.close_tab(a, window, cx);
        }
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(a) = self.active {
            self.activate_tab((a + 1) % self.tabs.len(), window, cx);
        }
    }

    /// Cmd/Ctrl+1 to 8: tab `n` (1-based), if open.
    fn activate_tab_number(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if n >= 1 && n <= self.tabs.len() {
            self.activate_tab(n - 1, window, cx);
        }
    }

    fn activate_last_tab(&mut self, _: &ActivateLastTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.tabs.len().checked_sub(1) {
            self.activate_tab(last, window, cx);
        }
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(a) = self.active {
            let len = self.tabs.len();
            self.activate_tab((a + len - 1) % len, window, cx);
        }
    }

    fn unload(&mut self, window: &mut Window) {
        self.player = None;
        self.ended = false;
        self.drawn_position = -1.0;
        for image in [self.frame.take(), self.cover.take()].into_iter().flatten() {
            let _ = window.drop_image(image);
        }
    }

    fn add_paths(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let first_new = self.playlist.len();
        // Absolute paths, so the saved session works from any directory.
        let absolute: Vec<PathBuf> = paths
            .iter()
            .map(|p| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.clone()))
            .collect();
        // A folder's files already listed (the folder added twice) are not
        // added again.
        let mut present: std::collections::HashSet<PathBuf> = self.playlist.iter().cloned().collect();
        for path in &absolute {
            if path.is_dir() {
                // Adding a folder again brings back files removed from it.
                self.removed_from_folders.retain(|p| !p.starts_with(path));
                self.folders.add(path.clone());
                for (file, stamp) in folders::list(path).unwrap_or_default() {
                    // Still being written: the rescan this schedules adds it.
                    if !folders::settled(&stamp) {
                        self.folders.rescan_after(folders::SETTLE);
                        continue;
                    }
                    self.stamps.insert(file.clone(), stamp);
                    if present.insert(file.clone()) {
                        self.playlist.push(file);
                    }
                }
            } else {
                // A file named on its own is added even if already listed.
                self.removed_from_folders.remove(path);
                present.insert(path.clone());
                self.playlist.push(path.clone());
            }
        }
        self.probe_new(cx);
        if self.player.is_none() && self.playlist.len() > first_new {
            self.play_index(first_new, window, cx);
        }
        cx.notify();
    }

    /// Read tags and durations for entries not seen yet, off the UI thread.
    fn probe_new(&mut self, cx: &mut Context<Self>) {
        let mut todo: Vec<PathBuf> = self.playlist.iter().filter(|p| !self.summaries.contains_key(*p)).cloned().collect();
        todo.dedup();
        if todo.is_empty() {
            return;
        }
        // Placeholders stop the same file being probed twice.
        for p in &todo {
            self.summaries.insert(p.clone(), Summary::default());
        }
        let probing = cx.background_executor().spawn(async move {
            todo.into_iter().map(|p| { let s = probe(&p).unwrap_or_default(); (p, s) }).collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let results = probing.await;
            let _ = this.update(cx, |this, cx| {
                this.summaries.extend(results);
                cx.notify();
            });
        })
        .detach();
    }

    /// The playlist label: the tag title, else the file name.
    fn display_name(&self, path: &Path) -> String {
        self.summaries.get(path).and_then(|s| s.title.clone()).unwrap_or_else(|| file_name(path))
    }

    /// The user removing an entry. One inside a followed folder stays out
    /// when the folder is rescanned.
    fn remove_track(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.playlist.get(ix)
            && self.folders.contains(path)
        {
            self.removed_from_folders.insert(path.clone());
        }
        self.drop_entry(ix, window, cx);
    }

    fn drop_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.playlist.len() {
            return;
        }
        self.playlist.remove(ix);
        // Tabs showing the removed entry close; later entries shift down.
        for t in (0..self.tabs.len()).rev() {
            let active = self.active == Some(t);
            match if active { self.current } else { self.tabs[t].track } {
                Some(c) if c == ix => self.close_tab(t, window, cx),
                Some(c) if c > ix && active => self.current = Some(c - 1),
                Some(c) if c > ix => self.tabs[t].track = Some(c - 1),
                _ => {}
            }
        }
        cx.notify();
    }

    fn still_duration(&self) -> Option<f64> {
        (self.still_seconds > 0).then_some(self.still_seconds as f64)
    }

    /// Set how long stills show, for every open file and those opened later.
    fn set_still_seconds(&mut self, secs: u32, cx: &mut Context<Self>) {
        self.still_seconds = secs;
        let duration = self.still_duration();
        let parked = self.tabs.iter_mut().filter_map(|t| t.parked.as_mut().and_then(|l| l.player.as_mut()));
        let preloaded = self.preloaded.as_mut().and_then(|(_, l)| l.player.as_mut());
        for p in self.player.iter_mut().chain(parked).chain(preloaded) {
            p.set_still_duration(duration);
        }
        cx.notify();
    }

    /// Turn upscaling on or off for every open file (and those opened
    /// later).
    fn set_upscaling(&mut self, on: bool, cx: &mut Context<Self>) {
        self.upscaling = on;
        let parked = self.tabs.iter().filter_map(|t| t.parked.as_ref().and_then(|l| l.player.as_ref()));
        let preloaded = self.preloaded.as_ref().and_then(|(_, l)| l.player.as_ref());
        for p in self.player.iter().chain(parked).chain(preloaded) {
            p.set_upscaling(on);
        }
        // A paused frame is decoded again, with or without upscaling.
        self.refreshed_for = None;
        self.limit_changed = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        cx.notify();
    }

    /// While paused nothing new is decoded, so the frame on screen keeps
    /// the size it was decoded at. Once the display size has settled (or
    /// upscaling was switched), decode it again by seeking to where it is.
    fn refresh_paused_frame(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        if !p.is_paused() || p.info.video.is_none() || self.limit_changed.elapsed() < Duration::from_millis(250) {
            return;
        }
        let key = (self.limit, self.upscaling);
        if self.refreshed_for == Some(key) {
            return;
        }
        self.refreshed_for = Some(key);
        // Only when the picture would actually change.
        let source = p.info.video.as_ref().map(|v| (v.width, v.height)).unwrap_or_default();
        let showing = match p.upscale_status() {
            UpscaleStatus::Active { to, .. } => Some(to),
            UpscaleStatus::TooSlow | UpscaleStatus::Failed(_) => return,
            _ => None,
        };
        let wanted = if self.upscaling { upscale::target_size(source, self.limit) } else { None };
        if showing != wanted {
            let at = p.position();
            p.seek(at);
        }
    }

    /// The info panel's words for what upscaling is doing.
    fn upscale_label(status: &UpscaleStatus) -> String {
        match status {
            UpscaleStatus::Off => "Off".into(),
            UpscaleStatus::NotNeeded => "Not needed".into(),
            UpscaleStatus::Active { from, to } => {
                let by = upscale::available().unwrap_or("GPU");
                format!("{} × {} → {} × {} using {by}", from.0, from.1, to.0, to.1)
            }
            UpscaleStatus::TooSlow => "Stopped: too slow".into(),
            UpscaleStatus::Failed(e) => format!("Stopped: {e}"),
        }
    }

    /// Insert an entry at `ix`, keeping every tab on its file.
    fn insert_entry(&mut self, ix: usize, path: PathBuf) {
        self.playlist.insert(ix, path);
        for t in 0..self.tabs.len() {
            let track = if self.active == Some(t) { &mut self.current } else { &mut self.tabs[t].track };
            if let Some(c) = track
                && *c >= ix
            {
                *c += 1;
            }
        }
    }

    /// Bring the playlist in line with a rescan of the followed folders:
    /// new files join next to their folder's other entries, in name order;
    /// files deleted from disk leave, unless open in a tab.
    fn apply_scan(&mut self, scan: Scan, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        for (root, listing) in scan.folders {
            // Missing or unreadable (an unmounted drive): leave it alone.
            let Some(files) = listing else { continue };
            // Cleared or replaced while the scan ran.
            if !self.folders.roots().contains(&root) {
                continue;
            }
            let on_disk: std::collections::HashSet<&PathBuf> = files.iter().map(|(p, _)| p).collect();
            let open: Vec<PathBuf> = self.tabs.iter().map(|t| t.path.clone()).collect();
            for ix in (0..self.playlist.len()).rev() {
                let p = &self.playlist[ix];
                if p.starts_with(&root) && !on_disk.contains(p) && !open.contains(p) && !p.exists() {
                    self.stamps.remove(p);
                    self.drop_entry(ix, window, cx);
                    changed = true;
                }
            }
            self.removed_from_folders.retain(|p| !p.starts_with(&root) || on_disk.contains(p));
            let present: std::collections::HashSet<PathBuf> = self.playlist.iter().cloned().collect();
            for (file, stamp) in files {
                // Still being written; `Folders` looks again shortly.
                if !folders::settled(&stamp) {
                    continue;
                }
                if self.stamps.insert(file.clone(), stamp).is_some_and(|old| old != stamp) {
                    // Changed since its tags were read.
                    self.summaries.remove(&file);
                    changed = true;
                }
                if present.contains(&file) || self.removed_from_folders.contains(&file) {
                    continue;
                }
                let in_folder: Vec<usize> = (0..self.playlist.len()).filter(|&i| self.playlist[i].starts_with(&root)).collect();
                let ix = in_folder
                    .iter()
                    .copied()
                    .find(|&i| self.playlist[i] > file)
                    .or(in_folder.last().map(|i| i + 1))
                    .unwrap_or(self.playlist.len());
                self.insert_entry(ix, file);
                changed = true;
            }
        }
        if changed {
            self.probe_new(cx);
            cx.notify();
        }
    }

    fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        if self.volume > 0.0 {
            self.muted = false;
        }
        if let Some(p) = &self.player {
            p.set_volume(self.volume, self.muted);
        }
        cx.notify();
    }

    fn seek_to(&mut self, secs: f64, cx: &mut Context<Self>) {
        if let Some(p) = self.player.as_mut() {
            p.seek(secs);
            if self.ended {
                self.ended = false;
            }
        }
        cx.notify();
    }

    // ---- actions ----------------------------------------------------------

    fn play_pause(&mut self, _: &PlayPause, window: &mut Window, cx: &mut Context<Self>) {
        match self.player.as_mut() {
            Some(p) if self.ended => {
                p.seek(0.0);
                p.set_paused(false);
                self.ended = false;
            }
            Some(p) => {
                let paused = !p.is_paused();
                p.set_paused(paused);
            }
            None if !self.playlist.is_empty() => self.play_index(self.current.unwrap_or(0), window, cx),
            None => {}
        }
        cx.notify();
    }

    fn next_track(&mut self, _: &NextTrack, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.is_empty() {
            return;
        }
        let next = self.current.map_or(0, |c| (c + 1) % self.playlist.len());
        self.play_index(next, window, cx);
    }

    fn previous_track(&mut self, _: &PreviousTrack, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.is_empty() {
            return;
        }
        // Like every player: restart the track unless we're near its start.
        if let Some(p) = self.player.as_mut()
            && p.position() > 3.0
        {
            p.seek(0.0);
            self.ended = false;
            cx.notify();
            return;
        }
        let len = self.playlist.len();
        let prev = self.current.map_or(0, |c| (c + len - 1) % len);
        self.play_index(prev, window, cx);
    }

    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(pos) = self.player.as_ref().map(Player::position) {
            self.seek_to(pos + SEEK_STEP, cx);
        }
    }

    fn seek_backward(&mut self, _: &SeekBackward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(pos) = self.player.as_ref().map(Player::position) {
            self.seek_to(pos - SEEK_STEP, cx);
        }
    }

    fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.set_volume(self.volume + VOLUME_STEP, cx);
    }

    fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.set_volume(self.volume - VOLUME_STEP, cx);
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        if let Some(p) = &self.player {
            p.set_volume(self.volume, self.muted);
        }
        cx.notify();
    }

    fn cycle_repeat(&mut self, _: &CycleRepeat, _: &mut Window, cx: &mut Context<Self>) {
        self.repeat = self.repeat.next();
        cx.notify();
    }

    fn open_files(&mut self, _: &OpenFiles, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Add to Playlist".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| this.add_paths(&paths, window, cx));
            }
        })
        .detach();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn toggle_info(&mut self, _: &ToggleInfoPanel, _: &mut Window, cx: &mut Context<Self>) {
        self.info_open = !self.info_open;
        cx.notify();
    }

    fn toggle_fullscreen(&mut self, _: &ToggleFullscreen, window: &mut Window, _: &mut Context<Self>) {
        window.toggle_fullscreen();
    }

    /// Stop: the file is closed along with its tab.
    fn stop(&mut self, _: &Stop, window: &mut Window, cx: &mut Context<Self>) {
        self.close_active_tab(&CloseTab, window, cx);
    }

    fn clear_playlist(&mut self, _: &ClearPlaylist, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_tabs(window);
        self.playlist.clear();
        self.folders.clear();
        self.removed_from_folders.clear();
        self.stamps.clear();
        cx.notify();
    }

    fn open_picker(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.as_ref().is_some_and(|p| p.kind == kind) {
            self.close_picker(window, cx);
            return;
        }
        self.settings = None;
        self.picker = Some(Picker { kind, query: String::new(), selected: 0 });
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn picker_items(&self, window: &Window) -> Vec<PickerItem> {
        let Some(picker) = &self.picker else { return Vec::new() };
        let mut items: Vec<(i32, PickerItem)> = match picker.kind {
            PickerKind::Commands => commands()
                .into_iter()
                .filter_map(|(label, action)| {
                    let m = fuzzy_match(&picker.query, label)?;
                    let keys = window.keystroke_text_for(action.as_ref());
                    let detail = (!keys.is_empty()).then(|| keys.into());
                    Some((
                        m.score,
                        PickerItem { label: label.into(), positions: m.positions, detail, target: PickerTarget::Action(action) },
                    ))
                })
                .collect(),
            PickerKind::Files => self
                .playlist
                .iter()
                .enumerate()
                .filter_map(|(ix, path)| {
                    let name = self.display_name(path);
                    let m = fuzzy_match(&picker.query, &name).or_else(|| {
                        // Also find tracks by file name; no highlight then.
                        fuzzy_match(&picker.query, &file_name(path)).map(|m| super::fuzzy::Match { positions: Vec::new(), ..m })
                    })?;
                    let artist = self.summaries.get(path).and_then(|s| s.artist.clone());
                    let detail = artist.or_else(|| path.parent().map(|p| p.display().to_string())).map(Into::into);
                    Some((m.score, PickerItem { label: name.into(), positions: m.positions, detail, target: PickerTarget::Track(ix) }))
                })
                .collect(),
        };
        if !picker.query.is_empty() {
            items.sort_by_key(|(score, _)| -score);
        }
        items.into_iter().map(|(_, item)| item).collect()
    }

    fn picker_up(&mut self, _: &PickerUp, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.picker_items(window).len();
        if let Some(p) = self.picker.as_mut()
            && len > 0
        {
            p.selected = (p.selected + len - 1) % len;
            cx.notify();
        }
    }

    fn picker_down(&mut self, _: &PickerDown, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.picker_items(window).len();
        if let Some(p) = self.picker.as_mut()
            && len > 0
        {
            p.selected = (p.selected + 1) % len;
            cx.notify();
        }
    }

    fn picker_confirm(&mut self, _: &PickerConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.picker.as_ref().map_or(0, |p| p.selected);
        let item = self.picker_items(window).into_iter().nth(selected);
        self.run_picker_item(item, false, window, cx);
    }

    fn picker_confirm_in_new_tab(&mut self, _: &PickerConfirmInNewTab, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.picker.as_ref().map_or(0, |p| p.selected);
        let item = self.picker_items(window).into_iter().nth(selected);
        self.run_picker_item(item, true, window, cx);
    }

    fn run_picker_item(&mut self, item: Option<PickerItem>, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.close_picker(window, cx);
        match item.map(|i| i.target) {
            Some(PickerTarget::Action(action)) => window.dispatch_action(action, cx),
            Some(PickerTarget::Track(ix)) if new_tab => self.open_in_new_tab(ix, window, cx),
            Some(PickerTarget::Track(ix)) => self.play_index(ix, window, cx),
            None => {}
        }
    }

    fn picker_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else { return };
        let ks = &event.keystroke;
        if ks.modifiers.control || ks.modifiers.platform {
            return;
        }
        if ks.key == "backspace" {
            picker.query.pop();
        } else if let Some(ch) = ks.key_char.as_ref().filter(|c| !c.chars().any(char::is_control)) {
            picker.query.push_str(ch);
        } else {
            return;
        }
        picker.selected = 0;
        cx.stop_propagation();
        cx.notify();
    }

    // ---- mouse ------------------------------------------------------------

    fn fraction_in(bounds: &Rc<Cell<Option<Bounds<Pixels>>>>, x: Pixels) -> Option<f64> {
        let b = bounds.get()?;
        let w = f32::from(b.size.width);
        (w > 0.0).then(|| (f32::from(x - b.origin.x) / w).clamp(0.0, 1.0) as f64)
    }

    fn start_drag(&mut self, drag: Drag, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.drag = Some(drag);
        self.update_drag(event.position.x, true, cx);
    }

    fn update_drag(&mut self, x: Pixels, force: bool, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Seek) => {
                let Some(f) = Self::fraction_in(&self.seek_bounds, x) else { return };
                self.drag_fraction = f;
                // Scrub: seek at most every 60 ms while dragging.
                if (force || self.last_drag_seek.elapsed() > Duration::from_millis(60))
                    && let Some(d) = self.player.as_ref().and_then(|p| p.info.duration)
                {
                    self.seek_to(f * d, cx);
                    self.last_drag_seek = Instant::now();
                }
                cx.notify();
            }
            Some(Drag::Volume) => {
                if let Some(f) = Self::fraction_in(&self.volume_bounds, x) {
                    self.set_volume(f as f32, cx);
                }
            }
            None => {}
        }
    }

    fn end_drag(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.update_drag(x, true, cx);
            self.drag = None;
            cx.notify();
        }
    }

    // ---- rendering --------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = self.player.as_ref().map(|p| p.info.title.clone()).or_else(|| {
            self.current.and_then(|ix| self.playlist.get(ix)).map(|p| file_name(p))
        });
        div()
            .id("title-bar")
            .h(px(34.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(Theme::title_bar_bg())
            .border_b_1()
            .border_color(Theme::border_variant())
            .pl(if cfg!(target_os = "macos") { px(80.) } else { px(12.) })
            .pr(px(8.))
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_sm()
                    .min_w_0()
                    .child(div().text_color(Theme::text()).font_weight(FontWeight::MEDIUM).child("airsplice-player"))
                    .when_some(title, |el, title| {
                        el.child(icon(Icon::ChevronRight, px(12.), Theme::text_placeholder()))
                            .child(div().text_color(Theme::text_muted()).truncate().child(title))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(icon_button("tb-find", Icon::Search, false, cx.listener(|this, _, window, cx| {
                        this.open_picker(PickerKind::Files, window, cx)
                    }))),
            )
            .into_any_element()
    }

    fn render_playlist(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .h(px(32.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(12.))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::text_muted())
                    .child({
                        // Stills count for the time they show, when timed.
                        let still = (self.still_seconds > 0).then_some(self.still_seconds as f64);
                        let durations: Option<f64> = self
                            .playlist
                            .iter()
                            .map(|p| {
                                let s = self.summaries.get(p);
                                s.and_then(|s| s.duration).or_else(|| still.filter(|_| s.is_some_and(|s| s.has_video) && is_picture(p)))
                            })
                            .sum();
                        match durations {
                            Some(total) if !self.playlist.is_empty() => {
                                format!("PLAYLIST  {}  ·  {}", self.playlist.len(), format_time(total))
                            }
                            _ => format!("PLAYLIST  {}", self.playlist.len()),
                        }
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .id("pl-autoplay")
                            .debug_selector(|| "pl-autoplay".into())
                            .px(px(6.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded(px(4.))
                            .text_xs()
                            .cursor_pointer()
                            .text_color(if self.autoplay { Theme::accent() } else { Theme::text_placeholder() })
                            .when(self.autoplay, |el| el.bg(Theme::selected()))
                            .hover(|s| s.bg(Theme::hover()))
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_autoplay(&ToggleAutoplay, window, cx)))
                            .child("Autoplay"),
                    )
                    .child(icon_button("pl-add", Icon::Plus, false, cx.listener(|this, _, window, cx| {
                        this.open_files(&OpenFiles, window, cx)
                    }))),
            );

        let body: AnyElement = if self.playlist.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .px(px(16.))
                .text_sm()
                .text_color(Theme::text_placeholder())
                .child(icon(Icon::Music, px(28.), Theme::text_placeholder()))
                .child(div().text_center().child("Drop files or folders here"))
                .into_any_element()
        } else {
            div()
                .id("playlist-items")
                .flex_1()
                .overflow_y_scroll()
                .py(px(2.))
                .children(self.playlist.iter().enumerate().map(|(ix, path)| {
                    let active = self.current == Some(ix);
                    let playing = active && self.player.as_ref().is_some_and(|p| !p.is_paused());
                    let summary = self.summaries.get(path);
                    let video = summary.is_some_and(|s| s.has_video);
                    let duration = summary.and_then(|s| s.duration);
                    let row_icon = if playing {
                        Icon::Play
                    } else if video {
                        Icon::Film
                    } else {
                        Icon::Music
                    };
                    div()
                        .id(("track", ix))
                        .group("track")
                        .relative()
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(12.))
                        .pr(px(10.))
                        .border_l_2()
                        .border_color(if active { Theme::accent() } else { gpui::transparent_black() })
                        .when(active, |el| el.bg(Theme::selected()))
                        .hover(|s| s.bg(Theme::hover()))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                            if e.modifiers().secondary() {
                                this.open_in_new_tab(ix, window, cx);
                            } else {
                                this.play_index(ix, window, cx);
                            }
                        }))
                        .child(icon(row_icon, px(14.), if active { Theme::accent() } else { Theme::text_muted() }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .truncate()
                                .text_color(if active { Theme::text() } else { Theme::text_muted() })
                                .child(self.display_name(path)),
                        )
                        .when_some(duration, |el, d| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .font_family(mono_font())
                                    .text_color(Theme::text_placeholder())
                                    .group_hover("track", |s| s.invisible())
                                    .child(format_time(d)),
                            )
                        })
                        .child(
                            div()
                                .absolute()
                                .right(px(4.))
                                .flex()
                                .invisible()
                                .group_hover("track", |s| s.visible())
                                .child(icon_button(("new-tab", ix), Icon::Plus, false, cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_in_new_tab(ix, window, cx);
                                })))
                                .child(icon_button(("remove", ix), Icon::Close, false, cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.remove_track(ix, window, cx);
                                }))),
                        )
                }))
                .into_any_element()
        };

        div()
            .w(px(240.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(Theme::panel_bg())
            .border_r_1()
            .border_color(Theme::border())
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let active = self.active == Some(ix);
            let (player, ended) = if active {
                (self.player.as_ref(), self.ended)
            } else {
                tab.parked.as_ref().map_or((None, false), |l| (l.player.as_ref(), l.ended))
            };
            let playing = active && !ended && player.is_some_and(|p| !p.is_paused());
            let glyph = if playing {
                Icon::Play
            } else if player.is_some_and(|p| p.info.video.is_some()) {
                Icon::Film
            } else {
                Icon::Music
            };
            let group = SharedString::from(format!("tab-{ix}"));
            div()
                .id(("tab", ix))
                .group(group.clone())
                .h_full()
                .max_w(px(220.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .pl(px(12.))
                .pr(px(6.))
                .border_r_1()
                .border_color(Theme::border())
                .text_sm()
                .cursor_pointer()
                .when(active, |el| el.bg(Theme::tab_active_bg()).text_color(Theme::text()))
                .when(!active, |el| el.text_color(Theme::text_muted()).hover(|s| s.bg(Theme::hover())))
                .on_click(cx.listener(move |this, _, window, cx| this.activate_tab(ix, window, cx)))
                .child(icon(glyph, px(14.), if playing { Theme::accent() } else { Theme::text_muted() }))
                .child(div().min_w_0().truncate().child(self.display_name(&tab.path)))
                .child(
                    div().when(!active, |el| el.invisible().group_hover(group, |s| s.visible())).child(icon_button(
                        ("tab-close", ix),
                        Icon::Close,
                        false,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(ix, window, cx);
                        }),
                    )),
                )
        });
        div()
            .id("tab-bar")
            .h(px(32.))
            .flex_none()
            .flex()
            .overflow_x_scroll()
            .bg(Theme::tab_bar_bg())
            .border_b_1()
            .border_color(Theme::border())
            .children(tabs)
            .into_any_element()
    }

    fn render_stage(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(frame) = &self.frame {
            return div()
                .id("video")
                .flex_1()
                .min_h_0()
                .bg(gpui::black())
                .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                    if e.click_count() == 2 {
                        window.toggle_fullscreen();
                    } else {
                        this.play_pause(&PlayPause, window, cx);
                    }
                }))
                .child(img(frame.clone()).size_full().object_fit(ObjectFit::Contain))
                .into_any_element();
        }
        let content: AnyElement = match (&self.player, &self.error) {
            (Some(player), _) => {
                let info = &player.info;
                let art: AnyElement = match &self.cover {
                    Some(cover) => img(cover.clone()).size(px(220.)).rounded(px(8.)).object_fit(ObjectFit::Cover).into_any_element(),
                    None => div()
                        .size(px(220.))
                        .rounded(px(8.))
                        .bg(Theme::panel_bg())
                        .border_1()
                        .border_color(Theme::border_variant())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(if info.video.is_some() { Icon::Film } else { Icon::Music }, px(64.), Theme::text_placeholder()))
                        .into_any_element(),
                };
                let byline = [info.artist.clone(), info.album.clone()].into_iter().flatten().collect::<Vec<_>>().join("  ·  ");
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.))
                    .max_w(px(520.))
                    .child(art)
                    .child(div().mt(px(8.)).text_xl().text_color(Theme::text()).text_center().child(info.title.clone()))
                    .when(!byline.is_empty(), |el| el.child(div().text_color(Theme::text_muted()).text_center().child(byline)))
                    .when_some(self.error.clone(), |el, e| el.child(div().text_sm().text_color(Theme::warning()).text_center().child(e)))
                    .into_any_element()
            }
            (None, Some(error)) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.))
                .max_w(px(520.))
                .child(icon(Icon::Info, px(32.), Theme::error()))
                .child(div().text_color(Theme::text()).text_center().child(error.clone()))
                .into_any_element(),
            (None, None) => self.render_welcome(),
        };
        div().flex_1().min_h_0().flex().items_center().justify_center().p(px(24.)).child(content).into_any_element()
    }

    fn render_welcome(&self) -> AnyElement {
        let row = |label: &'static str, keys: String| {
            div()
                .flex()
                .justify_between()
                .gap(px(32.))
                .text_sm()
                .child(div().text_color(Theme::text_muted()).child(label))
                .child(div().text_color(Theme::text_placeholder()).font_family(mono_font()).child(keys))
        };
        let (cmd, shift) = if cfg!(target_os = "macos") { ("⌘", "⇧") } else { ("Ctrl+", "Shift+") };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .w(px(340.))
            .child(div().text_2xl().text_color(Theme::text()).mb(px(2.)).child("airsplice-player"))
            .child(div().text_sm().text_color(Theme::text_placeholder()).mb(px(16.)).child("Drop audio, video or image files anywhere in the window."))
            .child(row("Open Files", format!("{cmd}O")))
            .child(row("Command Palette", format!("{shift}{cmd}P")))
            .child(row("Find in Playlist", format!("{cmd}P")))
            .child(row("Toggle Playlist", format!("{cmd}B")))
            .child(row("Toggle Info Panel", format!("{cmd}R")))
            .child(row("Play / Pause", "Space".into()))
            .into_any_element()
    }

    fn render_transport(&self, cx: &mut Context<Self>) -> AnyElement {
        let (position, duration, paused) = match &self.player {
            Some(p) => (p.position(), p.info.duration, p.is_paused() || self.ended),
            None => (0.0, None, true),
        };
        let fraction = match (self.drag, duration) {
            (Some(Drag::Seek), Some(_)) => self.drag_fraction,
            (_, Some(d)) if d > 0.0 => (position / d).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        let shown_volume = if self.muted { 0.0 } else { self.volume };
        let time = |s: &str| div().text_xs().font_family(mono_font()).text_color(Theme::text_muted()).w(px(56.)).child(s.to_string());

        let seek_row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(time(&format_time(position)).text_right())
            .child(
                slider("seek", fraction as f32, seek_bounds, duration.is_some(), self.drag == Some(Drag::Seek))
                    .flex_1()
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                        this.start_drag(Drag::Seek, e, cx)
                    })),
            )
            .child(time(&duration.map_or("--:--".into(), format_time)));

        let controls = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div().w(px(160.)).flex().items_center().gap(px(4.)).child(
                    icon_button(
                        "repeat",
                        if self.repeat == Repeat::One { Icon::RepeatOne } else { Icon::Repeat },
                        self.repeat != Repeat::Off,
                        cx.listener(|this, _, window, cx| this.cycle_repeat(&CycleRepeat, window, cx)),
                    ),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon_button("prev", Icon::SkipBack, false, cx.listener(|this, _, window, cx| {
                        this.previous_track(&PreviousTrack, window, cx)
                    })))
                    .child(
                        div()
                            .id("play")
                            .size(px(36.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(Theme::text())
                            .hover(|s| s.bg(gpui::white()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| this.play_pause(&PlayPause, window, cx)))
                            .child(icon(if paused { Icon::Play } else { Icon::Pause }, px(16.), Theme::editor_bg())),
                    )
                    .child(icon_button("next", Icon::SkipForward, false, cx.listener(|this, _, window, cx| {
                        this.next_track(&NextTrack, window, cx)
                    }))),
            )
            .child(
                div()
                    .w(px(160.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(6.))
                    .child(icon_button(
                        "mute",
                        if self.muted || self.volume == 0.0 { Icon::VolumeMuted } else { Icon::Volume },
                        false,
                        cx.listener(|this, _, window, cx| this.toggle_mute(&ToggleMute, window, cx)),
                    ))
                    .child(
                        slider("volume", shown_volume, volume_bounds, true, self.drag == Some(Drag::Volume))
                            .w(px(96.))
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                this.start_drag(Drag::Volume, e, cx)
                            }))
                            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                                let dy = f32::from(e.delta.pixel_delta(px(16.)).y);
                                this.set_volume(this.volume + dy / 400.0, cx);
                            })),
                    ),
            );

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(16.))
            .pt(px(10.))
            .pb(px(10.))
            .bg(Theme::panel_bg())
            .border_t_1()
            .border_color(Theme::border())
            .child(seek_row)
            .child(controls)
            .into_any_element()
    }

    fn render_info(&self) -> AnyElement {
        let section = |title: &'static str| {
            div()
                .mt(px(14.))
                .mb(px(4.))
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(Theme::text_placeholder())
                .child(title)
        };
        let row = |k: &'static str, v: String| {
            div()
                .flex()
                .gap(px(8.))
                .py(px(2.))
                .text_sm()
                .child(div().w(px(84.)).flex_none().text_color(Theme::text_muted()).child(k))
                .child(div().flex_1().min_w_0().text_color(Theme::text()).child(v))
        };
        let mut body = div().flex().flex_col().px(px(12.));
        match &self.player {
            None => {
                body = body.child(div().mt(px(12.)).text_sm().text_color(Theme::text_placeholder()).child("Nothing playing"));
            }
            Some(p) => {
                let info = &p.info;
                body = body.child(section("MEDIA")).child(row("Title", info.title.clone()));
                if let Some(a) = &info.artist {
                    body = body.child(row("Artist", a.clone()));
                }
                if let Some(a) = &info.album {
                    body = body.child(row("Album", a.clone()));
                }
                body = body.child(row("Duration", info.duration.map_or("Unknown".into(), format_time)));
                if let Some(v) = &info.video {
                    body = body
                        .child(section("VIDEO"))
                        .child(row("Codec", v.codec.into()))
                        .child(row("Size", format!("{} × {}", v.width, v.height)))
                        .child(row("Decoder", v.decoder.into()))
                        .child(row("Upscaling", Self::upscale_label(&p.upscale_status())));
                } else if let Some(why) = &info.video_unsupported {
                    body = body.child(section("VIDEO")).child(row("Status", why.clone()));
                }
                if let Some(a) = &info.audio {
                    body = body
                        .child(section("AUDIO"))
                        .child(row("Codec", a.codec.clone()))
                        .child(row("Sample rate", format!("{} Hz", a.sample_rate)))
                        .child(row("Channels", channel_label(a.channels)))
                        .child(row("Output", format!("{} Hz", a.output_rate)));
                    let [l, r] = p.levels();
                    body = body.child(section("LEVELS")).child(meter(l)).child(meter(r));
                }
                body = body.child(section("FILE")).child(
                    div().text_xs().text_color(Theme::text_muted()).child(info.path.display().to_string()),
                );
            }
        }
        div()
            .id("info-panel")
            .w(px(260.))
            .flex_none()
            .overflow_y_scroll()
            .bg(Theme::panel_bg())
            .border_l_1()
            .border_color(Theme::border())
            .child(
                div()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::text_muted())
                    .child("INFO"),
            )
            .child(body)
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = match &self.player {
            Some(_) if self.ended => "Ended",
            Some(p) if p.is_paused() => "Paused",
            Some(_) => "Playing",
            None => "Stopped",
        };
        let summary = self.player.as_ref().map(|p| {
            let mut parts = Vec::new();
            if let Some(v) = &p.info.video {
                let hw = if v.decoder.contains("hardware") { " (HW)" } else { "" };
                parts.push(format!("{}{hw} {}×{}", v.codec, v.width, v.height));
            }
            if let Some(a) = &p.info.audio {
                parts.push(format!("{} {} kHz {}", a.codec, fmt_khz(a.sample_rate), channel_label(a.channels)));
            }
            parts.join("  ·  ")
        });
        div()
            .h(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(6.))
            .bg(Theme::status_bar_bg())
            .border_t_1()
            .border_color(Theme::border_variant())
            .text_xs()
            .text_color(Theme::text_muted())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .child(icon_button("sb-playlist", Icon::PanelLeft, self.sidebar_open, cx.listener(|this, _, window, cx| {
                        this.toggle_sidebar(&ToggleSidebar, window, cx)
                    })))
                    .child(div().text_color(Theme::text()).child(state))
                    .when_some(summary, |el, s| el.child(div().truncate().child(s)))
                    .when_some(self.error.clone(), |el, e| el.child(div().truncate().text_color(Theme::error()).child(e)))
                    .when_some(self.notice.clone(), |el, n| el.child(div().truncate().text_color(Theme::warning()).child(n))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .flex_none()
                    .child(self.repeat.label())
                    .when_some(self.output_status(), |el, status| el.child(status))
                    .child(div().debug_selector(|| "sb-output".into()).child(icon_button("sb-output", Icon::Display, self.outputs_live(), cx.listener(|this, _, window, cx| {
                        this.toggle_outputs(&ToggleOutputs, window, cx)
                    }))))
                    .child(
                        div().debug_selector(|| "sb-settings".into()).child(icon_button("sb-settings", Icon::Settings, self.settings.is_some(), cx.listener(|this, _, window, cx| {
                            this.open_settings(&OpenSettings, window, cx)
                        }))),
                    )
                    .child(icon_button("sb-info", Icon::PanelRight, self.info_open, cx.listener(|this, _, window, cx| {
                        this.toggle_info(&ToggleInfoPanel, window, cx)
                    }))),
            )
            .into_any_element()
    }

    fn render_picker(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.picker.as_ref()?;
        let items = self.picker_items(window);
        let selected = picker.selected.min(items.len().saturating_sub(1));
        let placeholder = match picker.kind {
            PickerKind::Commands => "Execute a command…",
            PickerKind::Files => "Search playlist…",
        };
        let input = div()
            .h(px(40.))
            .flex()
            .items_center()
            .px(px(12.))
            .border_b_1()
            .border_color(Theme::border_variant())
            .child(if picker.query.is_empty() {
                div().flex().items_center().child(caret()).child(div().text_color(Theme::text_placeholder()).child(placeholder))
            } else {
                div().flex().items_center().child(div().text_color(Theme::text()).child(picker.query.clone())).child(caret())
            });
        let empty = items.is_empty();
        let list = div()
            .id("picker-list")
            .max_h(px(360.))
            .overflow_y_scroll()
            .py(px(4.))
            .children(items.into_iter().enumerate().map(|(ix, item)| {
                let highlights = item
                    .positions
                    .iter()
                    .map(|&p| {
                        let end = p + item.label[p..].chars().next().map_or(1, char::len_utf8);
                        (p..end, HighlightStyle { color: Some(Theme::accent()), ..Default::default() })
                    })
                    .collect::<Vec<_>>();
                let label = StyledText::new(item.label.clone()).with_highlights(highlights);
                let detail = item.detail.clone();
                let item = Rc::new(Cell::new(Some(item)));
                div()
                    .id(("picker-item", ix))
                    .mx(px(4.))
                    .px(px(8.))
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.))
                    .rounded(px(4.))
                    .text_sm()
                    .text_color(Theme::text())
                    .when(ix == selected, |el| el.bg(Theme::selected()))
                    .hover(|s| s.bg(Theme::hover()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        this.run_picker_item(item.take(), e.modifiers().secondary(), window, cx)
                    }))
                    .child(div().min_w_0().truncate().child(label))
                    .when_some(detail, |el, d| {
                        el.child(div().flex_none().max_w(px(220.)).truncate().text_xs().text_color(Theme::text_placeholder()).child(d))
                    })
            }))
            .when(empty, |el| el.child(div().px(px(12.)).py(px(8.)).text_sm().text_color(Theme::text_placeholder()).child("No matches")));

        Some(
            div()
                .id("picker-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                // A click outside only dismisses: it does not also reach
                // what is underneath (such as the button that opened this).
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_picker(window, cx)))
                .child(
                    div()
                        .id("picker")
                        .key_context(PICKER_CONTEXT)
                        .track_focus(&self.picker_focus)
                        .on_key_down(cx.listener(Self::picker_key))
                        .on_action(cx.listener(Self::picker_up))
                        .on_action(cx.listener(Self::picker_down))
                        .on_action(cx.listener(Self::picker_confirm))
                        .on_action(cx.listener(Self::picker_confirm_in_new_tab))
                        .on_action(cx.listener(|this, _: &PickerDismiss, window, cx| this.close_picker(window, cx)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .occlude()
                        .mt(px(72.))
                        .w(px(560.))
                        .max_h(px(420.))
                        .flex()
                        .flex_col()
                        .bg(Theme::elevated_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(8.))
                        .shadow_lg()
                        .overflow_hidden()
                        .child(input)
                        .child(list),
                )
                .into_any_element(),
        )
    }
}

impl Render for MediaPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(p) = &self.player {
            self.drawn_position = p.position();
        }
        let immersive = window.is_fullscreen() && self.frame.is_some();
        let picker = self.render_picker(window, cx);
        let settings = self.render_settings(window, cx);

        let workspace = div()
            .flex_1()
            .min_h_0()
            .flex()
            .when(self.sidebar_open && !immersive, |el| el.child(self.render_playlist(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .bg(Theme::editor_bg())
                    .when(!immersive, |el| el.child(self.render_tab_bar(cx)))
                    .child(self.render_stage(cx))
                    .child(self.render_transport(cx)),
            )
            .when(self.info_open && !immersive, |el| el.child(self.render_info()));

        // The player context (single-key shortcuts) wraps the workspace only;
        // the picker is a sibling so typing in it never triggers them.
        // Action handlers live on the root so both can dispatch to them.
        let player_area = div()
            .key_context(PLAYER_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .when(!immersive, |el| el.child(self.render_title_bar(cx)))
            .child(workspace)
            .when(!immersive, |el| el.child(self.render_status_bar(cx)));

        div()
            .id("root")
            .key_context("Workspace")
            .relative()
            .size_full()
            .bg(Theme::editor_bg())
            .text_color(Theme::text())
            .on_action(cx.listener(Self::play_pause))
            .on_action(cx.listener(Self::next_track))
            .on_action(cx.listener(Self::previous_track))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::volume_up))
            .on_action(cx.listener(Self::volume_down))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::cycle_repeat))
            .on_action(cx.listener(Self::open_files))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_info))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::toggle_outputs))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::toggle_autoplay))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::close_active_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(|this, _: &ActivateTab1, window, cx| this.activate_tab_number(1, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab2, window, cx| this.activate_tab_number(2, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab3, window, cx| this.activate_tab_number(3, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab4, window, cx| this.activate_tab_number(4, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab5, window, cx| this.activate_tab_number(5, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab6, window, cx| this.activate_tab_number(6, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab7, window, cx| this.activate_tab_number(7, window, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab8, window, cx| this.activate_tab_number(8, window, cx)))
            .on_action(cx.listener(Self::activate_last_tab))
            .on_action(cx.listener(Self::clear_playlist))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| this.open_picker(PickerKind::Commands, window, cx)))
            .on_action(cx.listener(|this, _: &FileFinder, window, cx| this.open_picker(PickerKind::Files, window, cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if e.pressed_button == Some(MouseButton::Left) {
                    this.update_drag(e.position.x, false, cx);
                } else if this.drag.is_some() {
                    // Released outside the window.
                    this.end_drag(e.position.x, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| this.end_drag(e.position.x, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                let paths = paths.paths().to_vec();
                this.add_paths(&paths, window, cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(Theme::hover()))
            .child(player_area)
            .children(picker)
            .children(settings)
    }
}

// ---- small components ------------------------------------------------------

fn icon(icon: Icon, size: Pixels, color: gpui::Hsla) -> impl IntoElement {
    svg().path(icon.path()).size(size).flex_none().text_color(color)
}

fn icon_button(
    id: impl Into<gpui::ElementId>,
    glyph: Icon,
    toggled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .when(toggled, |el| el.bg(Theme::selected()))
        .hover(|s| s.bg(Theme::hover()))
        .cursor_pointer()
        .on_click(on_click)
        .child(icon(glyph, px(14.), if toggled { Theme::accent() } else { Theme::text_muted() }))
}

/// A Zed-styled horizontal slider. Its track bounds are recorded during
/// layout so mouse positions can be mapped to a fraction.
fn slider(
    id: &'static str,
    fraction: f32,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    enabled: bool,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .id(id)
        .group(id)
        .h(px(16.))
        .flex()
        .items_center()
        .when(enabled, |el| el.cursor_pointer())
        .child(
            div()
                .relative()
                .w_full()
                .h(px(4.))
                .rounded_full()
                .bg(Theme::track())
                .child(canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
                .child(div().absolute().left_0().top_0().h_full().w(gpui::relative(fraction)).rounded_full().bg(if enabled {
                    Theme::accent()
                } else {
                    Theme::track()
                }))
                .when(enabled, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(-4.))
                            .left(gpui::relative(fraction))
                            .ml(px(-6.))
                            .size(px(12.))
                            .rounded_full()
                            .bg(Theme::text())
                            .when(!active, |k| k.invisible().group_hover(id, |s| s.visible())),
                    )
                }),
        )
}

fn meter(level: f32) -> impl IntoElement {
    let db = 20.0 * level.max(1e-5).log10();
    let fraction = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let color = if db > -3.0 {
        Theme::error()
    } else if db > -12.0 {
        Theme::warning()
    } else {
        Theme::success()
    };
    div()
        .my(px(3.))
        .h(px(6.))
        .w_full()
        .rounded_full()
        .bg(Theme::track())
        .child(div().h_full().w(gpui::relative(fraction)).rounded_full().bg(color))
}

fn caret() -> impl IntoElement {
    div().w(px(1.5)).h(px(18.)).mx(px(1.)).bg(Theme::accent())
}

// ---- helpers ---------------------------------------------------------------

fn to_render_image(width: u32, height: u32, bgra: Vec<u8>) -> Arc<RenderImage> {
    // GPUI treats the buffer as BGRA regardless of the image type's name.
    let buffer = image::RgbaImage::from_raw(width, height, bgra).expect("frame buffer matches its dimensions");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

fn image_to_render(image: &Image) -> Arc<RenderImage> {
    to_render_image(image.width, image.height, image.bgra.as_ref().clone())
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0).floor() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn fmt_khz(rate: u32) -> String {
    let s = format!("{:.3}", rate as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn channel_label(channels: usize) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n} ch"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(59.9), "0:59");
        assert_eq!(format_time(61.0), "1:01");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(-3.0), "0:00");
    }

    #[test]
    fn rates_and_channels() {
        assert_eq!(fmt_khz(48_000), "48");
        assert_eq!(fmt_khz(44_100), "44.1");
        assert_eq!(fmt_khz(22_050), "22.05");
        assert_eq!(fmt_khz(11_025), "11.025");
        assert_eq!(channel_label(6), "5.1");
        assert_eq!(channel_label(3), "3 ch");
    }

    #[test]
    fn repeat_cycles_through_all_modes() {
        assert_eq!(Repeat::Off.next().next().next(), Repeat::Off);
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    /// Drives the real view through the real keymap on GPUI's test platform.
    #[gpui::test]
    fn keyboard_drives_the_player(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let files = vec![fixture("video_only.mp4"), fixture("h264_mp3.mkv"), fixture("tagged.mp3")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        let state = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |v, _| (v.current, v.player.as_ref().map(|p| p.is_paused()), v.picker.as_ref().map(|p| p.query.clone())))
        };

        // The first file starts playing as soon as it is added.
        assert_eq!(state(cx), (Some(0), Some(false), None));
        cx.simulate_keystrokes("space");
        assert_eq!(state(cx).1, Some(true), "space pauses");
        cx.simulate_keystrokes("space");
        assert_eq!(state(cx).1, Some(false), "space resumes");

        cx.simulate_keystrokes("n");
        assert_eq!(state(cx).0, Some(1), "n plays the next track");
        cx.simulate_keystrokes("p");
        assert_eq!(state(cx).0, Some(0), "p near the start goes to the previous track");

        // Command palette: open, filter, run.
        cx.simulate_keystrokes("secondary-shift-p");
        assert_eq!(state(cx).2.as_deref(), Some(""), "palette opens");
        cx.simulate_keystrokes("n e x t space t");
        assert_eq!(state(cx).2.as_deref(), Some("next t"), "typing filters, space types a space");
        assert_eq!(state(cx).1, Some(false), "typing in the palette does not trigger player keys");
        cx.simulate_keystrokes("backspace");
        assert_eq!(state(cx).2.as_deref(), Some("next "));
        cx.simulate_keystrokes("enter");
        assert_eq!(state(cx), (Some(1), state(cx).1, None), "palette ran Next Track and closed");

        // File finder: jump to a track by name.
        cx.simulate_keystrokes("secondary-p");
        cx.simulate_keystrokes("t a g g");
        cx.simulate_keystrokes("enter");
        assert_eq!(state(cx).0, Some(2), "file finder played tagged.mp3");

        // Escape dismisses without running anything.
        cx.simulate_keystrokes("secondary-shift-p");
        cx.simulate_keystrokes("q u i t escape");
        assert_eq!(state(cx).2, None);
        assert_eq!(state(cx).0, Some(2));

        // Arrow keys move the palette selection; the selection wraps.
        cx.simulate_keystrokes("secondary-shift-p");
        cx.simulate_keystrokes("up");
        let len = view.update_in(cx, |v, window, _| v.picker_items(window).len());
        assert_eq!(view.read_with(cx, |v, _| v.picker.as_ref().unwrap().selected), len - 1);
        cx.simulate_keystrokes("escape");

        // Player keys.
        cx.simulate_keystrokes("r");
        assert_eq!(view.read_with(cx, |v, _| v.repeat), Repeat::All);
        cx.simulate_keystrokes("m");
        assert!(view.read_with(cx, |v, _| v.muted));
        cx.simulate_keystrokes("up");
        assert!(!view.read_with(cx, |v, _| v.muted), "raising the volume unmutes");
        let before = view.read_with(cx, |v, _| v.volume);
        cx.simulate_keystrokes("down down");
        let after = view.read_with(cx, |v, _| v.volume);
        assert!((before - after - 2.0 * VOLUME_STEP).abs() < 1e-6, "{before} -> {after}");

        // Panels.
        cx.simulate_keystrokes("secondary-b");
        assert!(!view.read_with(cx, |v, _| v.sidebar_open));
        cx.simulate_keystrokes("secondary-r");
        assert!(view.read_with(cx, |v, _| v.info_open));
    }

    #[gpui::test]
    fn removing_tracks_keeps_the_current_track_pointer_right(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let files = vec![fixture("video_only.mp4"), fixture("video_only.mp4"), fixture("video_only.mp4")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        view.update_in(cx, |v, window, cx| v.play_index(2, window, cx));
        view.update_in(cx, |v, window, cx| v.remove_track(0, window, cx));
        assert_eq!(view.read_with(cx, |v, _| (v.current, v.playlist.len())), (Some(1), 2));
        view.update_in(cx, |v, window, cx| v.remove_track(1, window, cx));
        assert_eq!(view.read_with(cx, |v, _| (v.current, v.player.is_some())), (None, false), "removing the playing track stops it");
    }

    #[gpui::test]
    fn an_unplayable_file_shows_an_error_and_keeps_the_app_usable(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let bogus = std::env::temp_dir().join("airsplice-player-not-media.mp3");
        std::fs::write(&bogus, b"this is not audio").unwrap();
        let files = vec![bogus.clone(), fixture("video_only.mp4")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        let error = view.read_with(cx, |v, _| v.error.clone()).expect("error shown");
        assert!(error.contains("airsplice-player-not-media.mp3"), "{error}");
        cx.simulate_keystrokes("n");
        assert!(view.read_with(cx, |v, _| v.player.is_some() && v.error.is_none()));
        let _ = std::fs::remove_file(bogus);
    }

    #[gpui::test]
    fn the_session_is_restored_paused_where_it_was_left(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let dir = std::env::temp_dir().join(format!("airsplice-player-session-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(dir.join("session.json"));
        // Video-only files, so this runs without an audio device.
        let files = vec![fixture("video_only.mp4"), fixture("bframes_h264.mp4")];

        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(files, Some(store), window, cx)
        });
        vcx.simulate_keystrokes("n r r m secondary-b");
        view.update_in(vcx, |v, _, cx| {
            v.player.as_mut().unwrap().seek(1.5);
            v.set_volume(0.4, cx);
            v.save_session(true);
        });

        // A fresh launch with no files restores everything, paused.
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(Vec::new(), Some(store), window, cx)
        });
        view.read_with(vcx, |v, _| {
            assert_eq!(v.playlist.len(), 2);
            assert_eq!(v.current, Some(1));
            let p = v.player.as_ref().expect("current track reopened");
            assert!(p.is_paused(), "restored paused");
            assert!((p.position() - 1.5).abs() < 0.01, "restored at {}", p.position());
            assert_eq!((v.volume, v.muted, v.repeat, v.sidebar_open), (0.4, false, Repeat::One, false));
        });

        // Files on the command line replace the playlist but keep settings.
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(vec![fixture("video_only.mp4")], Some(store), window, cx)
        });
        view.read_with(vcx, |v, _| {
            assert_eq!((v.playlist.len(), v.current, v.volume), (1, Some(0), 0.4));
            assert!(!v.player.as_ref().unwrap().is_paused(), "command-line files autoplay");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn playlist_shows_tag_titles_and_finds_tracks_by_title(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let files = vec![fixture("video_only.mp4"), fixture("tagged.mp3")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        // Background probing finishes when the executor is idle.
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.read_with(cx, |v, _| v.summaries.get(&fixture("tagged.mp3")).is_none_or(|s| s.duration.is_none())) {
            assert!(Instant::now() < deadline, "probe did not finish");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        view.read_with(cx, |v, _| {
            assert_eq!(v.display_name(&fixture("tagged.mp3")), "Test Tone");
            assert_eq!(v.display_name(&fixture("video_only.mp4")), "video_only.mp4", "no title tag: file name");
            assert!(v.summaries[&fixture("video_only.mp4")].has_video);
        });
        cx.simulate_keystrokes("secondary-p t e s t space t o n e enter");
        assert_eq!(view.read_with(cx, |v, _| v.current), Some(1), "found by tag title");
    }

    #[gpui::test]
    fn added_paths_are_stored_absolute(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let cwd = std::env::current_dir().unwrap();
        let relative = fixture("video_only.mp4").strip_prefix(&cwd).map(Path::to_path_buf).expect("fixture under cwd");
        assert!(relative.is_relative());
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(vec![relative], None, window, cx));
        view.read_with(cx, |v, _| {
            assert!(v.playlist[0].is_absolute(), "{:?}", v.playlist[0]);
            assert!(v.player.is_some());
        });
    }

    /// Run the view's tick until `done` holds, within a bounded budget.
    fn tick_until(view: &gpui::Entity<MediaPlayer>, cx: &mut gpui::VisualTestContext, what: &str, done: impl Fn(&MediaPlayer) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !view.read_with(cx, |v, _| done(v)) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            view.update_in(cx, |v, window, cx| v.tick(window, cx));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn parked(v: &MediaPlayer, ix: usize) -> &Loaded {
        v.tabs[ix].parked.as_ref().expect("a background tab")
    }

    #[gpui::test]
    fn tabs_cue_files_in_the_background_and_cut_between_them(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        // Video-only files, so this runs without an audio device.
        let files = vec![fixture("video_only.mp4"), fixture("bframes_h264.mp4")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        assert_eq!(view.read_with(cx, |v, _| (v.tabs.len(), v.active)), (1, Some(0)), "the first file opens in a tab");

        // Open in a new tab: cued paused in the background, nothing else changes.
        view.update_in(cx, |v, window, cx| v.open_in_new_tab(1, window, cx));
        view.read_with(cx, |v, _| {
            assert_eq!((v.tabs.len(), v.active, v.current), (2, Some(0), Some(0)));
            assert!(!v.player.as_ref().unwrap().is_paused(), "the current tab keeps playing");
            assert!(parked(v, 1).player.as_ref().unwrap().is_paused(), "the new tab waits, paused");
        });
        tick_until(&view, cx, "the cued frame", |v| parked(v, 1).frame.is_some());

        // Switching while playing cuts: the new tab plays at once, showing its
        // cued frame, and the old one pauses where it was.
        cx.simulate_keystrokes("secondary-shift-]");
        let left_at = view.read_with(cx, |v, _| {
            assert_eq!((v.active, v.current), (Some(1), Some(1)));
            assert!(v.frame.is_some(), "a picture straight away");
            assert!(!v.player.as_ref().unwrap().is_paused(), "a cut keeps playing");
            let old = parked(v, 0).player.as_ref().unwrap();
            assert!(old.is_paused());
            old.position()
        });
        std::thread::sleep(Duration::from_millis(150));
        let still = view.read_with(cx, |v, _| parked(v, 0).player.as_ref().unwrap().position());
        assert_eq!(still, left_at, "a background tab holds its position");

        // Switching while paused stays paused.
        cx.simulate_keystrokes("space secondary-shift-[");
        view.read_with(cx, |v, _| {
            assert_eq!((v.active, v.current), (Some(0), Some(0)));
            assert!(v.player.as_ref().unwrap().is_paused());
            assert_eq!(v.player.as_ref().unwrap().position(), left_at, "back where it was left");
        });

        // Cmd+number picks a tab; 9 is the last one; a number past the open
        // tabs does nothing.
        view.update_in(cx, |v, window, cx| v.open_in_new_tab(0, window, cx));
        cx.simulate_keystrokes("secondary-3");
        assert_eq!(view.read_with(cx, |v, _| v.active), Some(2));
        cx.simulate_keystrokes("secondary-1");
        assert_eq!(view.read_with(cx, |v, _| v.active), Some(0));
        cx.simulate_keystrokes("secondary-5");
        assert_eq!(view.read_with(cx, |v, _| v.active), Some(0), "no fifth tab");
        cx.simulate_keystrokes("secondary-9");
        assert_eq!(view.read_with(cx, |v, _| v.active), Some(2), "9 is the last tab");
        // Closing the active tab shows its neighbour, paused; closing the last
        // leaves nothing open. Tabs now: entries 0, 1, 0.
        cx.simulate_keystrokes("secondary-1 secondary-w");
        view.read_with(cx, |v, _| {
            assert_eq!((v.tabs.len(), v.active, v.current), (2, Some(0), Some(1)), "entry 1's tab moved into first place");
            assert!(v.player.as_ref().unwrap().is_paused());
        });
        cx.simulate_keystrokes("secondary-w");
        assert_eq!(view.read_with(cx, |v, _| (v.tabs.len(), v.active, v.current)), (1, Some(0), Some(0)));
        cx.simulate_keystrokes("secondary-w");
        view.read_with(cx, |v, _| assert_eq!((v.tabs.len(), v.active, v.current, v.player.is_some()), (0, None, None, false)));
    }

    #[gpui::test]
    fn tabs_stop_at_the_limit_and_the_file_finder_opens_into_them(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let files = vec![fixture("video_only.mp4"), fixture("bframes_h264.mp4")];
        let (view, cx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        // Cmd+Enter in the file finder opens the match in a new tab.
        cx.simulate_keystrokes("secondary-p b f r a secondary-enter");
        view.read_with(cx, |v, _| {
            assert_eq!((v.tabs.len(), v.active), (2, Some(0)));
            assert_eq!(v.tabs[1].path, fixture("bframes_h264.mp4"));
        });
        for _ in 2..MAX_TABS {
            view.update_in(cx, |v, window, cx| v.open_in_new_tab(0, window, cx));
        }
        assert_eq!(view.read_with(cx, |v, _| (v.tabs.len(), v.notice.is_none())), (MAX_TABS, true));
        view.update_in(cx, |v, window, cx| v.open_in_new_tab(0, window, cx));
        view.read_with(cx, |v, _| {
            assert_eq!(v.tabs.len(), MAX_TABS, "no tab past the limit");
            assert!(v.notice.as_deref().is_some_and(|n| n.contains("close one")), "{:?}", v.notice);
        });
        // Clearing the playlist closes every tab.
        view.update_in(cx, |v, window, cx| v.clear_playlist(&ClearPlaylist, window, cx));
        assert_eq!(view.read_with(cx, |v, _| (v.tabs.len(), v.active, v.player.is_some())), (0, None, false));
    }

    #[gpui::test]
    fn removing_a_playlist_entry_closes_its_tabs_and_renumbers_the_rest(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (a, b, c) = (fixture("video_only.mp4"), fixture("bframes_h264.mp4"), fixture("ctts_v0_negative.mov"));
        let (view, cx) = cx.add_window_view({
            let files = vec![a.clone(), b.clone(), c.clone()];
            |window, cx| MediaPlayer::new(files, None, window, cx)
        });
        view.update_in(cx, |v, window, cx| {
            v.open_in_new_tab(2, window, cx);
            v.open_in_new_tab(1, window, cx);
        });
        view.update_in(cx, |v, window, cx| v.remove_track(1, window, cx));
        view.read_with(cx, |v, _| {
            assert_eq!(v.tabs.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), vec![a.clone(), c.clone()], "b's tab closed");
            assert_eq!((v.active, v.current, v.tabs[1].track), (Some(0), Some(0), Some(1)), "c moved up to entry 1");
        });
        view.update_in(cx, |v, window, cx| v.remove_track(0, window, cx));
        view.read_with(cx, |v, _| {
            assert_eq!((v.tabs.len(), v.active, v.current), (1, Some(0), Some(0)));
            assert_eq!(v.playlist[0], c);
            assert_eq!(v.player.as_ref().unwrap().info.path, c, "c's tab is now on screen");
        });
    }

    /// A fresh folder under the temp dir, by its canonical path (what the
    /// playlist stores; on macOS `/var` is really `/private/var`).
    fn media_folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("airsplice-player-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// Copy a fixture into `dir` as `name`, dated an hour ago so it counts
    /// as completely written.
    fn put(dir: &Path, name: &str, fixture_name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::copy(fixture(fixture_name), &path).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        path
    }

    fn names(v: &MediaPlayer) -> Vec<String> {
        v.playlist.iter().map(|p| file_name(p)).collect()
    }

    #[gpui::test]
    fn a_folder_in_the_playlist_follows_files_added_and_deleted(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let dir = media_folder("follow");
        put(&dir, "b.mp4", "video_only.mp4");
        let d = put(&dir, "d.mp4", "bframes_h264.mp4");
        let (view, cx) = cx.add_window_view({
            let dir = dir.clone();
            |window, cx| MediaPlayer::new(vec![dir], None, window, cx)
        });
        view.update_in(cx, |v, window, cx| v.play_index(1, window, cx));
        assert_eq!(view.read_with(cx, |v, _| (names(v), v.current)), (vec!["b.mp4".into(), "d.mp4".into()], Some(1)));

        // A new file joins in name order; the playing entry keeps its file.
        put(&dir, "c.mp4", "video_only.mp4");
        tick_until(&view, cx, "c.mp4 to appear", |v| v.playlist.len() == 3);
        view.read_with(cx, |v, _| {
            assert_eq!(names(v), ["b.mp4", "c.mp4", "d.mp4"]);
            assert_eq!(v.current, Some(2));
            assert_eq!(v.player.as_ref().unwrap().info.path, d);
        });

        // In a subfolder too, and its tags are read.
        std::fs::create_dir(dir.join("sub")).unwrap();
        put(&dir, "sub/e.mp3", "tagged.mp3");
        tick_until(&view, cx, "sub/e.mp3 to appear", |v| v.playlist.len() == 4);
        cx.run_until_parked();
        view.read_with(cx, |v, _| {
            assert_eq!(v.playlist[3], dir.join("sub/e.mp3"));
            assert!(v.summaries[&v.playlist[3]].title.is_some(), "probed");
        });

        // Removed by the user: stays out when the folder changes again.
        view.update_in(cx, |v, window, cx| v.remove_track(1, window, cx));
        put(&dir, "a.mp4", "video_only.mp4");
        tick_until(&view, cx, "a.mp4 to appear", |v| v.playlist.first().is_some_and(|p| p.ends_with("a.mp4")));
        assert_eq!(view.read_with(cx, |v, _| names(v)), ["a.mp4", "b.mp4", "d.mp4", "e.mp3"], "c.mp4 not back");

        // Deleted from disk: leaves, unless it is open in a tab.
        std::fs::remove_file(dir.join("b.mp4")).unwrap();
        std::fs::remove_file(&d).unwrap();
        tick_until(&view, cx, "b.mp4 to leave", |v| v.playlist.len() == 3);
        view.read_with(cx, |v, _| {
            assert_eq!(names(v), ["a.mp4", "d.mp4", "e.mp3"], "d.mp4 is still on screen");
            assert_eq!((v.current, v.player.as_ref().unwrap().info.path.clone()), (Some(1), d.clone()));
        });

        // Adding the folder again brings back what was removed, once.
        view.update_in(cx, |v, window, cx| v.add_paths(std::slice::from_ref(&dir), window, cx));
        assert_eq!(view.read_with(cx, |v, _| names(v)), ["a.mp4", "d.mp4", "e.mp3", "c.mp4"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_file_still_being_written_waits_until_it_is_done(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let dir = media_folder("settle");
        put(&dir, "a.mp4", "video_only.mp4");
        let (view, cx) = cx.add_window_view({
            let dir = dir.clone();
            |window, cx| MediaPlayer::new(vec![dir], None, window, cx)
        });
        let fresh = dir.join("b.mp4");
        std::fs::copy(fixture("video_only.mp4"), &fresh).unwrap();
        std::fs::File::options().write(true).open(&fresh).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
        // The same scan, before and after the copy has settled.
        let scan = |stamp: Stamp| Scan { folders: vec![(dir.clone(), Some(vec![(dir.join("a.mp4"), (1, None)), (fresh.clone(), stamp)]))] };
        let len = std::fs::metadata(&fresh).unwrap().len();
        view.update_in(cx, |v, window, cx| v.apply_scan(scan((len, Some(std::time::SystemTime::now()))), window, cx));
        assert_eq!(view.read_with(cx, |v, _| names(v)), ["a.mp4"], "not added mid-copy");
        let done = std::time::SystemTime::now() - folders::SETTLE;
        view.update_in(cx, |v, window, cx| v.apply_scan(scan((len, Some(done))), window, cx));
        assert_eq!(view.read_with(cx, |v, _| names(v)), ["a.mp4", "b.mp4"]);
        // Through the watcher as well: picked up once it has been left alone.
        put(&dir, "c.mp4", "video_only.mp4");
        tick_until(&view, cx, "c.mp4 to appear", |v| v.playlist.len() == 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn files_added_while_the_app_was_closed_appear_at_launch(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let dir = media_folder("relaunch");
        let store = SessionStore::new(media_folder("relaunch-session").join("session.json"));
        put(&dir, "a.mp4", "video_only.mp4");
        put(&dir, "b.mp4", "video_only.mp4");
        let (view, vcx) = cx.add_window_view({
            let (dir, store) = (dir.clone(), store.clone());
            |window, cx| MediaPlayer::new(vec![dir], Some(store), window, cx)
        });
        view.update_in(vcx, |v, window, cx| {
            v.remove_track(1, window, cx);
            v.save_session(true);
        });
        vcx.update(|window, _| window.remove_window());

        put(&dir, "c.mp4", "video_only.mp4");
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(Vec::new(), Some(store), window, cx)
        });
        tick_until(&view, vcx, "c.mp4 to appear", |v| v.playlist.len() == 2);
        assert_eq!(view.read_with(vcx, |v, _| names(v)), ["a.mp4", "c.mp4"], "b.mp4 stays removed");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    fn output_windows(view: &gpui::Entity<MediaPlayer>, cx: &mut gpui::VisualTestContext) -> Vec<WindowHandle<OutputView>> {
        view.read_with(cx, |v, _| v.outputs.iter().map(|o| o.window).collect())
    }

    /// GPUI's test platform has one display, the one the window is on.
    fn test_display(view: &gpui::Entity<MediaPlayer>, cx: &mut gpui::VisualTestContext) -> String {
        view.update_in(cx, |v, window, cx| v.screens(window, cx))[0].uuid.clone()
    }

    #[gpui::test]
    fn settings_turn_outputs_on_and_off(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (view, vcx) = cx.add_window_view(|window, cx| MediaPlayer::new(vec![fixture("video_only.mp4")], None, window, cx));
        tick_until(&view, vcx, "the first frame", |v| v.frame.is_some());
        let display = test_display(&view, vcx);

        // With no display chosen, the switch opens the settings to pick one.
        vcx.simulate_keystrokes("secondary-shift-f");
        assert!(view.read_with(vcx, |v, _| v.settings.is_some()), "the switch opens settings");
        vcx.simulate_keystrokes("escape");
        vcx.simulate_keystrokes("secondary-,");
        assert!(view.read_with(vcx, |v, _| v.settings.is_some()), "so does Cmd+,");

        // The display the controls are on is offered too.
        let screens = view.update_in(vcx, |v, window, cx| v.screens(window, cx));
        assert_eq!((screens.len(), screens[0].label.as_str(), screens[0].here), (1, "Display 1", true));

        // Down to it (past upscaling and stills), Enter turns it on: the
        // picture goes there and follows playback.
        vcx.simulate_keystrokes("down down enter");
        vcx.run_until_parked();
        let out = *output_windows(&view, vcx).first().expect("output open");
        assert_eq!(vcx.windows().len(), 2);
        let shows_current = |vcx: &mut gpui::VisualTestContext| {
            let frame = view.read_with(vcx, |v, _| v.frame.clone()).unwrap();
            let _ = out.update(vcx, |_, window, _| window.refresh());
            vcx.run_until_parked();
            vcx.update(|_, cx| out.read(cx).unwrap().shown().is_some_and(|s| Arc::ptr_eq(s, &frame)))
        };
        assert!(shows_current(vcx), "first frame");
        let first = view.read_with(vcx, |v, _| v.frame.clone()).unwrap();
        tick_until(&view, vcx, "a later frame", |v| v.frame.as_ref().is_some_and(|f| !Arc::ptr_eq(f, &first)));
        assert!(shows_current(vcx), "later frame");
        assert_eq!(view.read_with(vcx, |v, _| v.output_status()).as_deref(), Some("Output: Display 1"));

        // Enter again turns it off.
        vcx.simulate_keystrokes("enter");
        assert_eq!(view.read_with(vcx, |v, _| (v.outputs.len(), v.output_setting.len())), (0, 0));
        assert_eq!(vcx.windows().len(), 1, "output window closed");
        vcx.simulate_keystrokes("escape");

        // The status bar switch: off keeps the choice, on brings it back.
        view.update_in(vcx, |v, window, cx| v.toggle_output(display.clone(), window, cx));
        vcx.run_until_parked();
        assert_eq!(output_windows(&view, vcx).len(), 1);
        vcx.simulate_keystrokes("secondary-shift-f");
        assert!(output_windows(&view, vcx).is_empty(), "switched off");
        view.read_with(vcx, |v, _| {
            assert_eq!(v.output_setting, std::slice::from_ref(&display), "the choice is kept");
            assert_eq!(v.output_status().as_deref(), Some("Outputs off"));
            assert!(!v.outputs_live());
        });
        vcx.simulate_keystrokes("secondary-shift-f");
        vcx.run_until_parked();
        assert_eq!(output_windows(&view, vcx).len(), 1, "switched back on");
        view.update_in(vcx, |v, window, cx| v.toggle_output(display.clone(), window, cx));
        assert!(output_windows(&view, vcx).is_empty());

        // Several outputs; one display not connected: the connected one
        // shows, the other is remembered and waited for.
        view.update_in(vcx, |v, window, cx| {
            v.toggle_output("unplugged".into(), window, cx);
            v.toggle_output(display.clone(), window, cx);
        });
        vcx.run_until_parked();
        assert_eq!(output_windows(&view, vcx).len(), 1);
        assert_eq!(view.read_with(vcx, |v, _| v.output_status()).as_deref(), Some("Outputs: Display 1, 1 not connected"));
        assert_eq!(view.read_with(vcx, |v, _| v.snapshot(false).output_displays), ["unplugged".to_string(), display.clone()]);
        view.update_in(vcx, |v, window, cx| v.toggle_output(display.clone(), window, cx));
        assert_eq!(view.read_with(vcx, |v, _| v.output_status()).as_deref(), Some("Output: display not connected"));

        // An output window closed from outside counts as turning it off.
        view.update_in(vcx, |v, window, cx| v.toggle_output(display.clone(), window, cx));
        vcx.run_until_parked();
        let out = output_windows(&view, vcx)[0];
        let _ = out.update(vcx, |_, window, _| window.remove_window());
        vcx.run_until_parked();
        view.update_in(vcx, |v, window, cx| v.sync_output(window, cx));
        assert_eq!(view.read_with(vcx, |v, _| (v.outputs.len(), v.output_setting.clone())), (0, vec!["unplugged".to_string()]));
        assert!(view.read_with(vcx, |v, _| v.player.is_some()), "playback carries on");
    }

    #[gpui::test]
    fn upscaling_is_a_setting_that_reaches_the_picture(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (view, vcx) = cx.add_window_view(|window, cx| MediaPlayer::new(vec![fixture("video_only.mp4")], None, window, cx));
        tick_until(&view, vcx, "the first frame", |v| v.frame.is_some());
        let frame_size = |view: &gpui::Entity<MediaPlayer>, vcx: &mut gpui::VisualTestContext| {
            view.read_with(vcx, |v, _| v.frame.as_ref().map(|f| f.size(0)).map(|s| (s.width.0 as u32, s.height.0 as u32)).unwrap())
        };
        let (source, limit) = view.read_with(vcx, |v, _| {
            let video = v.player.as_ref().unwrap().info.video.as_ref().unwrap();
            ((video.width, video.height), v.limit)
        });
        assert_eq!(frame_size(&view, vcx), source, "off by default: frames at their own size");

        // Settings: the switch is the first row, Enter flips it.
        vcx.simulate_keystrokes("secondary-, enter");
        let Some(name) = upscale::available() else {
            // No GPU (Docker, CI): the switch stays off and says why.
            assert!(!view.read_with(vcx, |v, _| v.upscaling), "nothing to switch on");
            eprintln!("skipping the rest: no GPU upscaler here");
            return;
        };
        eprintln!("upscaling with {name}");
        assert!(view.read_with(vcx, |v, _| v.upscaling && v.snapshot(false).upscaling), "on, and saved");
        vcx.simulate_keystrokes("escape");

        // Frames now arrive upscaled to fill the window, up to 2x.
        let target = upscale::target_size(source, limit).expect("the small fixture gains from upscaling in the test window");
        tick_until(&view, vcx, "an upscaled frame", |v| v.frame.as_ref().is_some_and(|f| f.size(0).width.0 as u32 == target.0));
        assert_eq!(frame_size(&view, vcx), target);
        let status = view.read_with(vcx, |v, _| v.player.as_ref().unwrap().upscale_status());
        assert_eq!(status, UpscaleStatus::Active { from: source, to: target });
        assert!(MediaPlayer::upscale_label(&status).contains(name), "the info panel names the upscaler");

        // Paused, then switched off: the frame on screen goes back to the
        // source size without playing.
        view.update_in(vcx, |v, window, cx| {
            v.play_pause(&PlayPause, window, cx);
            v.set_upscaling(false, cx);
        });
        tick_until(&view, vcx, "the paused frame at its own size", |v| v.frame.as_ref().is_some_and(|f| f.size(0).width.0 as u32 == source.0));
        assert!(view.read_with(vcx, |v, _| v.player.as_ref().unwrap().is_paused()), "still paused");
    }

    #[gpui::test]
    fn an_animated_gif_scrubs_in_the_player(cx: &mut gpui::TestAppContext) {
        use crate::media::picture::tests::{animated_gif, colour, temp_dir};
        cx.update(bind_keys);
        let dir = temp_dir("app");
        let gif = animated_gif(&dir);
        let (view, vcx) = cx.add_window_view({
            let gif = gif.clone();
            |window, cx| MediaPlayer::new(vec![gif], None, window, cx)
        });
        let shows = |i: usize| {
            let [r, g, b, a] = colour(i);
            move |v: &MediaPlayer| v.frame.as_ref().and_then(|f| f.as_bytes(0)).is_some_and(|px| px[..4] == [b, g, r, a])
        };
        tick_until(&view, vcx, "the first frame", shows(0));
        assert_eq!(view.read_with(vcx, |v, _| v.player.as_ref().unwrap().info.duration), Some(1.0), "a timeline, like a video");

        // Paused, then scrubbed: the frame on screen follows the seek bar.
        view.update_in(vcx, |v, window, cx| v.play_pause(&PlayPause, window, cx));
        for (target, frame) in [(0.65, 3), (0.35, 2), (0.15, 1)] {
            view.update_in(vcx, |v, _, cx| v.seek_to(target, cx));
            tick_until(&view, vcx, &format!("frame {frame} at {target}s"), shows(frame));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn stills_show_for_the_set_time_then_the_playlist_moves_on(cx: &mut gpui::TestAppContext) {
        use crate::media::picture::tests::temp_dir;
        cx.update(bind_keys);
        let dir = temp_dir("still-playlist");
        let still = dir.join("slide.png");
        image::RgbaImage::from_pixel(16, 9, image::Rgba([90, 90, 90, 255])).save(&still).unwrap();
        let (view, vcx) = cx.add_window_view({
            let files = vec![still.clone(), fixture("video_only.mp4")];
            |window, cx| MediaPlayer::new(files, None, window, cx)
        });
        tick_until(&view, vcx, "the still on screen", |v| v.frame.is_some());
        assert_eq!(view.read_with(vcx, |v, _| (v.still_seconds, v.player.as_ref().unwrap().info.duration)), (10, Some(10.0)), "10 s by default");

        // Settings: the stills row is second; Left and Right change it by a
        // second, digits type it. Saved, and applied to the still on screen.
        vcx.simulate_keystrokes("secondary-, down right");
        assert_eq!(view.read_with(vcx, |v, _| v.still_seconds), 11);
        vcx.simulate_keystrokes("left left");
        assert_eq!(view.read_with(vcx, |v, _| (v.still_seconds, v.snapshot(false).still_seconds)), (9, 9));
        vcx.simulate_keystrokes("4 7");
        assert_eq!(view.read_with(vcx, |v, _| (v.still_seconds, v.player.as_ref().unwrap().info.duration)), (47, Some(47.0)));
        vcx.simulate_keystrokes("backspace");
        assert_eq!(view.read_with(vcx, |v, _| v.still_seconds), 4);
        vcx.simulate_keystrokes("1 0 escape");
        assert_eq!(view.read_with(vcx, |v, _| (v.still_seconds, v.settings.is_none())), (410, true));

        // At 1 s the playlist moves on to the video by itself.
        view.update_in(vcx, |v, _, cx| v.set_still_seconds(1, cx));
        tick_until(&view, vcx, "the playlist to move past the still", |v| v.current == Some(1));
        assert_eq!(view.read_with(vcx, |v, _| v.player.as_ref().unwrap().info.path.clone()), fixture("video_only.mp4"));

        // Until skipped: the still stays up.
        view.update_in(vcx, |v, window, cx| {
            v.set_still_seconds(0, cx);
            v.play_index(0, window, cx);
        });
        tick_until(&view, vcx, "the still again", |v| v.frame.is_some() && v.current == Some(0));
        assert_eq!(view.read_with(vcx, |v, _| v.player.as_ref().unwrap().info.duration), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Three one-colour stills in a fresh folder.
    fn slides(name: &str) -> (PathBuf, Vec<PathBuf>) {
        let dir = crate::media::picture::tests::temp_dir(name);
        let paths = (0..3)
            .map(|i| {
                let p = dir.join(format!("slide{i}.png"));
                image::RgbaImage::from_pixel(16, 9, image::Rgba([40 * i as u8, 90, 90, 255])).save(&p).unwrap();
                p
            })
            .collect();
        (dir, paths)
    }

    #[gpui::test]
    fn autoplay_decides_whether_the_playlist_moves_on(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (dir, files) = slides("autoplay");
        let (view, vcx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        view.update_in(vcx, |v, _, cx| v.set_still_seconds(1, cx));
        assert!(view.read_with(vcx, |v, _| v.autoplay), "on by default");
        tick_until(&view, vcx, "the second slide", |v| v.current == Some(1));

        // Off (the playlist pane's switch): the slide ends and stays.
        vcx.run_until_parked();
        let switch = vcx.debug_bounds("pl-autoplay").expect("autoplay switch in the playlist pane").center();
        vcx.simulate_click(switch, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| !v.autoplay && !v.snapshot(false).autoplay), "off, and saved");
        tick_until(&view, vcx, "the slide to end", |v| v.ended);
        assert_eq!(view.read_with(vcx, |v, _| v.current), Some(1), "stayed on it");

        // Repeat one still loops with autoplay off.
        view.update_in(vcx, |v, window, cx| {
            v.repeat = Repeat::One;
            v.play_index(1, window, cx);
        });
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(1600) {
            view.update_in(vcx, |v, window, cx| v.tick(window, cx));
            assert!(!view.read_with(vcx, |v, _| v.ended), "repeat one stopped");
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_next_entry_is_preloaded_so_moving_on_is_instant(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (dir, files) = slides("preload");
        let (view, vcx) = cx.add_window_view(|window, cx| MediaPlayer::new(files, None, window, cx));
        view.update_in(vcx, |v, _, cx| v.set_still_seconds(2, cx));
        // While the first slide shows, the second is opened and cued.
        tick_until(&view, vcx, "the next slide preloaded and cued", |v| {
            v.preloaded.as_ref().is_some_and(|(p, l)| *p == v.playlist[1] && l.frame.is_some())
        });
        tick_until(&view, vcx, "the playlist to move on", |v| v.current == Some(1));
        // Its picture is on screen in the same tick: nothing was loaded.
        assert!(view.read_with(vcx, |v, _| v.frame.is_some() && v.player.as_ref().is_some_and(|p| !p.is_paused())));
        // The preload moves on to the entry after.
        tick_until(&view, vcx, "the third slide preloaded", |v| v.preloaded.as_ref().is_some_and(|(p, _)| *p == v.playlist[2]));

        // Nothing comes after the last entry (repeat off), so nothing is
        // preloaded; with autoplay off neither.
        view.update_in(vcx, |v, window, cx| v.play_index(2, window, cx));
        view.update_in(vcx, |v, window, cx| {
            v.opened_at = Instant::now() - Duration::from_secs(5);
            v.tick(window, cx);
        });
        assert!(view.read_with(vcx, |v, _| v.preloaded.is_none()));
        view.update_in(vcx, |v, window, cx| {
            v.autoplay = false;
            v.play_index(0, window, cx);
            v.opened_at = Instant::now() - Duration::from_secs(5);
            v.tick(window, cx);
        });
        assert!(view.read_with(vcx, |v, _| v.preloaded.is_none()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_footer_gear_opens_and_closes_settings(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (view, vcx) = cx.add_window_view(|window, cx| MediaPlayer::new(Vec::new(), None, window, cx));
        vcx.run_until_parked();
        let gear = vcx.debug_bounds("sb-settings").expect("gear in the footer").center();
        vcx.simulate_click(gear, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| v.settings.is_some()), "a click opens settings");
        vcx.simulate_click(gear, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| v.settings.is_none()), "a second click closes them");

        // The monitor button with nothing chosen opens settings too; a
        // second click (on the backdrop) closes them.
        let monitor = vcx.debug_bounds("sb-output").expect("monitor button in the footer").center();
        vcx.simulate_click(monitor, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| v.settings.is_some()));
        vcx.simulate_click(monitor, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| v.settings.is_none()));

        // With a display chosen it is a plain on/off switch.
        let display = test_display(&view, vcx);
        view.update_in(vcx, |v, window, cx| v.toggle_output(display, window, cx));
        vcx.run_until_parked();
        vcx.simulate_click(monitor, gpui::Modifiers::none());
        assert!(view.read_with(vcx, |v, _| !v.outputs_on && v.outputs.is_empty() && v.settings.is_none()));
        vcx.simulate_click(monitor, gpui::Modifiers::none());
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |v, _| v.outputs_on && v.outputs.len() == 1));
    }

    #[gpui::test]
    fn the_output_display_is_remembered(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let store = SessionStore::new(media_folder("output-session").join("session.json"));
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(vec![fixture("video_only.mp4")], Some(store), window, cx)
        });
        let display = test_display(&view, vcx);
        view.update_in(vcx, |v, window, cx| {
            v.toggle_output(display.clone(), window, cx);
            v.save_session(true);
        });
        vcx.run_until_parked();
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(Vec::new(), Some(store), window, cx)
        });
        view.update_in(vcx, |v, window, cx| v.tick(window, cx));
        vcx.run_until_parked();
        assert_eq!(output_windows(&view, vcx).len(), 1, "the output reopens at launch");

        // Switched off when the app closed: stays off, choice kept.
        view.update_in(vcx, |v, window, cx| {
            v.toggle_outputs(&ToggleOutputs, window, cx);
            v.save_session(true);
        });
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(Vec::new(), Some(store), window, cx)
        });
        view.update_in(vcx, |v, window, cx| v.tick(window, cx));
        vcx.run_until_parked();
        assert!(output_windows(&view, vcx).is_empty());
        assert_eq!(view.read_with(vcx, |v, _| v.output_status()).as_deref(), Some("Outputs off"));
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[gpui::test]
    fn open_tabs_are_restored_with_their_positions(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let dir = std::env::temp_dir().join(format!("airsplice-player-tabs-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(dir.join("session.json"));
        let files = vec![fixture("video_only.mp4"), fixture("bframes_h264.mp4")];
        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(files, Some(store), window, cx)
        });
        view.update_in(vcx, |v, window, cx| {
            v.player.as_mut().unwrap().seek(1.5);
            v.open_in_new_tab(1, window, cx);
            v.activate_tab(1, window, cx);
            v.player.as_mut().unwrap().seek(0.8);
            v.save_session(true);
        });

        let (view, vcx) = cx.add_window_view({
            let store = store.clone();
            |window, cx| MediaPlayer::new(Vec::new(), Some(store), window, cx)
        });
        view.read_with(vcx, |v, _| {
            assert_eq!((v.tabs.len(), v.active, v.current), (2, Some(1), Some(1)));
            let p = v.player.as_ref().unwrap();
            assert!(p.is_paused(), "restored paused");
            assert!((p.position() - 0.8).abs() < 0.01, "active tab at {}", p.position());
            let other = parked(v, 0).player.as_ref().unwrap();
            assert!((other.position() - 1.5).abs() < 0.01, "background tab at {}", other.position());
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
