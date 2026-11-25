//! The settings screen, and the external outputs it configures.
//!
//! Outputs are chosen per display in the settings, by each display's
//! stable id, and any number can be on at once, all showing the same
//! picture. The status bar button switches them all on and off. While a chosen
//! display is connected the picture is full screen on it; when it is
//! unplugged its window closes, and it comes back when the display does,
//! without anyone touching the controls.
//!
//! Sound goes to one audio output device: the system default unless one
//! is chosen here, also by stable id. A chosen device that is not
//! connected falls back to the default.

use super::*;
use gpui::{anchored, deferred, relative};

pub(super) const SETTINGS_CONTEXT: &str = "Settings";

/// The longest a still can be set to show, in seconds (an hour).
pub(super) const MAX_STILL_SECONDS: u32 = 3600;

/// `secs` moved by `by` seconds, within 0 (until skipped) and the maximum.
pub(super) fn step_still(secs: u32, by: i64) -> u32 {
    (secs as i64 + by).clamp(0, MAX_STILL_SECONDS as i64) as u32
}

/// The seconds typed so far after `key` (a digit or backspace); `None`
/// when the key does not edit a number.
pub(super) fn type_still(typed: &str, key: &str) -> Option<String> {
    let mut typed = typed.to_string();
    match key {
        "backspace" => {
            typed.pop();
        }
        d if d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()) => {
            if typed == "0" {
                typed.clear();
            }
            typed.push_str(d);
            if typed.parse::<u32>().is_ok_and(|v| v > MAX_STILL_SECONDS) {
                typed = MAX_STILL_SECONDS.to_string();
            }
        }
        _ => return None,
    }
    Some(typed)
}

fn still_label(secs: u32) -> String {
    if secs == 0 { "Until skipped".into() } else { format!("{secs} s") }
}

/// Settings rows: upscaling, stills, the dissolve, one per display, then
/// the audio output.
const DISSOLVE_ROW: usize = 2;
const FIRST_SCREEN_ROW: usize = 3;

/// How often the connected displays are checked, so an output display
/// plugged in or out is noticed.
const DISPLAY_POLL: Duration = Duration::from_millis(500);

pub(super) struct SettingsScreen {
    pub(super) selected: usize,
    /// Digits typed into the still duration, while editing it.
    pub(super) typed: Option<String>,
    /// The audio output list, while it is open: the highlighted choice
    /// (0 for the system default, then one per device).
    pub(super) audio_menu: Option<usize>,
}

/// A connected display, as the settings name it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Screen {
    /// Stable across reconnects and restarts; what the setting stores.
    pub(super) uuid: String,
    pub(super) label: String,
    pub(super) size: String,
    /// The controls are on this display.
    pub(super) here: bool,
}

/// How often the audio output devices are listed while the settings are
/// open, so one plugged in or out shows up.
const AUDIO_DEVICE_POLL: Duration = Duration::from_secs(2);

/// One fullscreen output window and the display it is on.
pub(super) struct Output {
    pub(super) window: WindowHandle<OutputView>,
    pub(super) uuid: String,
    pub(super) label: String,
}

/// Which output windows to open and close so the outputs follow the
/// setting: one on every chosen display that is connected, none elsewhere.
fn plan(chosen: &[String], connected: &[&str], open: &[&str]) -> (Vec<String>, Vec<String>) {
    let want: Vec<&str> = chosen.iter().map(String::as_str).filter(|c| connected.contains(c)).collect();
    let to_open = want.iter().filter(|w| !open.contains(w)).map(|w| w.to_string()).collect();
    let to_close = open.iter().filter(|o| !want.contains(o)).map(|o| o.to_string()).collect();
    (to_open, to_close)
}

fn display_for(uuid: &str, cx: &App) -> Option<DisplayId> {
    cx.displays().into_iter().find(|d| d.uuid().is_ok_and(|u| u.to_string() == uuid)).map(|d| d.id())
}

impl MediaPlayer {
    /// Connected displays, the primary one first, numbered in that order.
    pub(super) fn screens(&self, window: &Window, cx: &App) -> Vec<Screen> {
        let mut displays = cx.displays();
        let primary = cx.primary_display().map(|d| d.id());
        displays.sort_by_key(|d| Some(d.id()) != primary);
        let here = window.display(cx).map(|d| d.id());
        displays
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let uuid = d.uuid().inspect_err(|e| log::debug!("display {:?} has no stable id: {e}", d.id())).ok()?;
                let size = d.bounds().size;
                Some(Screen {
                    uuid: uuid.to_string(),
                    label: format!("Display {}", i + 1),
                    size: format!("{} × {}", f32::from(size.width).round(), f32::from(size.height).round()),
                    here: Some(d.id()) == here,
                })
            })
            .collect()
    }

    pub(super) fn output_on(&self, uuid: &str) -> bool {
        self.output_setting.iter().any(|u| u == uuid)
    }

    /// Turn the output on one display on or off. Takes effect now and is
    /// remembered.
    pub(super) fn toggle_output(&mut self, uuid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.output_on(&uuid) {
            self.output_setting.retain(|u| *u != uuid);
        } else {
            self.output_setting.push(uuid);
        }
        self.notice = None;
        self.sync_output(window, cx);
        cx.notify();
    }

    /// Whether any output is meant to be showing: switched on, with at
    /// least one display chosen.
    pub(super) fn outputs_live(&self) -> bool {
        self.outputs_on && !self.output_setting.is_empty()
    }

    /// The status bar button: switch the outputs on or off, keeping the
    /// chosen displays. With none chosen yet, it opens the settings to
    /// choose some.
    pub(super) fn toggle_outputs(&mut self, _: &ToggleOutputs, window: &mut Window, cx: &mut Context<Self>) {
        if self.output_setting.is_empty() {
            if self.settings.is_none() {
                self.open_settings(&OpenSettings, window, cx);
            }
            return;
        }
        self.outputs_on = !self.outputs_on;
        self.notice = None;
        self.sync_output(window, cx);
        cx.notify();
    }

    /// Open and close output windows to match the setting and the displays
    /// connected now.
    pub(super) fn sync_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.output_checked = Instant::now();
        // Closed by something other than us: take that as turning that
        // output off, rather than reopening it.
        let windows: Vec<_> = cx.windows().iter().map(|w| w.window_id()).collect();
        let (alive, closed): (Vec<Output>, Vec<Output>) =
            std::mem::take(&mut self.outputs).into_iter().partition(|o| windows.contains(&o.window.window_id()));
        self.outputs = alive;
        for out in closed {
            self.output_setting.retain(|u| *u != out.uuid);
            cx.notify();
        }

        // The biggest output window, for the size video is decoded and
        // upscaled to.
        let sizes: Vec<(u32, u32)> = self
            .outputs
            .iter()
            .filter_map(|o| {
                o.window
                    .update(cx, |_, w, _| {
                        let (size, scale) = (w.viewport_size(), w.scale_factor());
                        ((f32::from(size.width) * scale) as u32, (f32::from(size.height) * scale) as u32)
                    })
                    .ok()
            })
            .collect();
        self.outputs_px = sizes.into_iter().max_by_key(|(w, h)| *w as u64 * *h as u64).unwrap_or((0, 0));

        let screens = self.screens(window, cx);
        let connected: Vec<&str> = screens.iter().map(|s| s.uuid.as_str()).collect();
        // Opening counts as open, so a slow open is not started twice.
        let open: Vec<&str> = self.outputs.iter().map(|o| o.uuid.as_str()).chain(self.outputs_opening.iter().map(String::as_str)).collect();
        let chosen: &[String] = if self.outputs_on { &self.output_setting } else { &[] };
        let (to_open, to_close) = plan(chosen, &connected, &open);
        for uuid in to_close {
            let Some(ix) = self.outputs.iter().position(|o| o.uuid == uuid) else { continue };
            let out = self.outputs.remove(ix);
            let _ = out.window.update(cx, |_, window, _| window.remove_window());
            // Still chosen, so its display went away. Closing beats letting
            // the OS move the window onto another display.
            if self.outputs_on && self.output_on(&out.uuid) {
                self.notice = Some(format!("{} disconnected", out.label));
            }
            cx.notify();
        }
        for uuid in to_open {
            let label = screens.iter().find(|s| s.uuid == uuid).map_or_else(String::new, |s| s.label.clone());
            self.open_output(uuid, label, window, cx);
        }
    }

    fn open_output(&mut self, uuid: String, label: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(display) = display_for(&uuid, cx) else { return };
        self.outputs_opening.push(uuid.clone());
        let main = window.window_handle();
        let me = cx.entity();
        // The new window draws at once and reads this view, so it opens
        // once this update is over.
        cx.defer(move |cx| {
            let opened = output::open(display, &me, main, cx);
            me.update(cx, |this, cx| {
                this.outputs_opening.retain(|u| *u != uuid);
                match opened {
                    Ok(handle) if this.outputs_on && this.output_on(&uuid) => this.outputs.push(Output { window: handle, uuid, label }),
                    // Switched off while it opened.
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                    // Not retried: the user turns it on again if they want it.
                    Err(e) => {
                        this.output_setting.retain(|u| *u != uuid);
                        this.notice = Some(format!("Couldn't open the output on {label}: {e}"));
                    }
                }
                cx.notify();
            });
            // Keep the keyboard on the controls.
            let _ = main.update(cx, |_, window, _| window.activate_window());
        });
    }

    /// Called every tick; looks at the displays every [`DISPLAY_POLL`].
    pub(super) fn poll_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.output_checked.elapsed() >= DISPLAY_POLL {
            self.sync_output(window, cx);
        }
    }

    /// The footer's words for the outputs, when any are chosen.
    pub(super) fn output_status(&self) -> Option<String> {
        if self.output_setting.is_empty() {
            return None;
        }
        if !self.outputs_on {
            return Some("Outputs off".into());
        }
        let mut parts: Vec<String> = self.outputs.iter().map(|o| o.label.clone()).collect();
        let shown = self.outputs.len() + self.outputs_opening.len();
        let missing = self.output_setting.len().saturating_sub(shown);
        if missing > 0 {
            parts.push(if missing == 1 && shown == 0 { "display not connected".into() } else { format!("{missing} not connected") });
        }
        if parts.is_empty() {
            return None;
        }
        let noun = if self.output_setting.len() > 1 { "Outputs" } else { "Output" };
        Some(format!("{noun}: {}", parts.join(", ")))
    }

    /// The detail shown next to a display in the settings.
    pub(super) fn screen_detail(&self, s: &Screen) -> String {
        if s.here { format!("{}, covers this window", s.size) } else { s.size.clone() }
    }

    // ---- audio output -------------------------------------------------------

    /// List the audio output devices again.
    pub(super) fn refresh_audio_devices(&mut self) {
        self.audio_devices = audio::output_devices();
        self.audio_devices_at = Instant::now();
    }

    /// Called every tick; lists the audio outputs every
    /// [`AUDIO_DEVICE_POLL`] while the settings are open.
    pub(super) fn poll_audio_devices(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_some() && self.audio_devices_at.elapsed() >= AUDIO_DEVICE_POLL {
            let before = self.audio_devices.clone();
            self.refresh_audio_devices();
            if self.audio_devices != before {
                cx.notify();
            }
        }
    }

    /// Send sound to the device with stable id `id`, or the system default
    /// for `None`. Remembered. Every open file with sound holds a stream on
    /// the old device, so each is reopened where it was.
    pub(super) fn set_audio_output(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.audio_output == id {
            return;
        }
        self.audio_output = id;
        self.end_dissolve(window);
        let has_sound = |l: &Loaded| l.player.as_ref().is_some_and(|p| p.info.audio.is_some());

        let installed = self.take_installed();
        let installed = if has_sound(&installed) { self.reopen(installed, false, window) } else { installed };
        self.install(installed);
        for ix in 0..self.tabs.len() {
            if let Some(parked) = self.tabs[ix].parked.take() {
                let parked = if has_sound(&parked) { self.reopen(parked, true, window) } else { parked };
                self.tabs[ix].parked = Some(parked);
            }
        }
        // Opened again, on the new device, when next wanted.
        if let Some((_, l)) = self.preloaded.take_if(|(_, l)| has_sound(l)) {
            l.release(window);
        }
        self.update_title(window);
        cx.notify();
    }

    /// `old` opened again from where it was, playing or paused as it was.
    /// Its picture stays up until the new one decodes a frame.
    fn reopen(&self, old: Loaded, background: bool, window: &mut Window) -> Loaded {
        let Some(player) = &old.player else { return old };
        let (path, position, paused) = (player.info.path.clone(), player.position(), player.is_paused());
        let mut new = if background { self.load_in_background(&path, position) } else { self.load(&path, !paused, position) };
        // If it could not open again, its error says why.
        if new.player.is_some() {
            new.ended = old.ended;
        }
        new.frame = old.frame;
        if let Some(cover) = old.cover {
            let _ = window.drop_image(cover);
        }
        new
    }

    /// The audio output choices: the system default, then each device
    /// connected, as (stable id, name).
    fn audio_choices(&self) -> Vec<(Option<String>, String)> {
        std::iter::once((None, "System default".to_string()))
            .chain(self.audio_devices.iter().map(|d| (Some(d.id.clone()), d.name.clone())))
            .collect()
    }

    /// Where the chosen output is in [`Self::audio_choices`]; the system
    /// default when the chosen device is not connected.
    fn audio_choice_ix(&self) -> usize {
        self.audio_output.as_ref().and_then(|id| self.audio_devices.iter().position(|d| d.id == *id)).map_or(0, |i| i + 1)
    }

    /// Open the audio output list, highlighting the current choice, or
    /// close it.
    fn toggle_audio_menu(&mut self, cx: &mut Context<Self>) {
        let current = self.audio_choice_ix();
        if let Some(s) = self.settings.as_mut() {
            s.audio_menu = if s.audio_menu.is_some() { None } else { Some(current) };
            cx.notify();
        }
    }

    /// Choose audio output `ix` from the list, and close it.
    fn pick_audio(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.settings.as_mut() {
            s.audio_menu = None;
        }
        if let Some((id, _)) = self.audio_choices().into_iter().nth(ix) {
            self.set_audio_output(id, window, cx);
        }
        cx.notify();
    }

    // ---- settings screen ----------------------------------------------------

    pub(super) fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(window, cx);
            return;
        }
        self.picker = None;
        self.refresh_audio_devices();
        self.settings = Some(SettingsScreen { selected: 0, typed: None, audio_menu: None });
        window.focus(&self.settings_focus);
        cx.notify();
    }

    /// Select a settings row, ending any typing.
    fn settings_select(&mut self, ix: usize) {
        if let Some(s) = self.settings.as_mut() {
            s.selected = ix;
            s.typed = None;
            s.audio_menu = None;
        }
    }

    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn settings_move(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        // Up and Down move through the audio output list while it is open.
        let choices = self.audio_choices().len() as isize;
        if let Some(s) = self.settings.as_mut()
            && let Some(at) = s.audio_menu.as_mut()
        {
            *at = (*at as isize + by).rem_euclid(choices) as usize;
            cx.notify();
            return;
        }
        let len = (FIRST_SCREEN_ROW + self.screens(window, cx).len() + 1) as isize;
        if let Some(s) = self.settings.as_mut()
            && len > 0
        {
            s.selected = (s.selected as isize + by).rem_euclid(len) as usize;
            s.typed = None;
            cx.notify();
        }
    }

    fn settings_toggle(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Enter in the open audio output list picks the highlighted one.
        if let Some(at) = self.settings.as_ref().and_then(|s| s.audio_menu) {
            self.pick_audio(at, window, cx);
            return;
        }
        let was_typing = self.settings.as_mut().and_then(|s| {
            s.selected = ix;
            s.typed.take()
        });
        match ix {
            0 => {
                // Nothing to switch on without a GPU that can do it.
                if upscale::available().is_some() {
                    self.set_upscaling(!self.upscaling, cx);
                }
            }
            DISSOLVE_ROW => self.set_dissolve(!self.dissolve, window, cx),
            // Enter on the stills row starts typing seconds, or ends it.
            1 => {
                if was_typing.is_none()
                    && let Some(s) = self.settings.as_mut()
                {
                    s.typed = Some(String::new());
                }
                cx.notify();
            }
            _ => {
                let screens = self.screens(window, cx);
                let first_audio = FIRST_SCREEN_ROW + screens.len();
                if ix == first_audio {
                    self.toggle_audio_menu(cx);
                } else if let Some(screen) = screens.into_iter().nth(ix - FIRST_SCREEN_ROW) {
                    self.toggle_output(screen.uuid, window, cx);
                }
            }
        }
    }

    /// Left and Right: a second less or more on the stills row, half a
    /// second on the dissolve's.
    fn settings_adjust(&mut self, by: i64, cx: &mut Context<Self>) {
        match self.settings.as_mut() {
            Some(s) if s.selected == 1 => {
                s.typed = None;
                self.set_still_seconds(step_still(self.still_seconds, by), cx);
            }
            Some(s) if s.selected == DISSOLVE_ROW => self.set_dissolve_secs(dissolve::step_dissolve(self.dissolve_secs, by), cx),
            _ => {}
        }
    }

    /// Digits typed while the stills row is selected set the seconds
    /// directly; Backspace deletes one. Applied as you type.
    fn settings_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt {
            return;
        }
        let Some(s) = self.settings.as_mut().filter(|s| s.selected == 1) else { return };
        let Some(typed) = type_still(s.typed.as_deref().unwrap_or(""), &ks.key) else { return };
        let secs = typed.parse().unwrap_or(0);
        s.typed = Some(typed);
        cx.stop_propagation();
        self.set_still_seconds(secs, cx);
    }

    pub(super) fn render_settings(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let settings = self.settings.as_ref()?;
        let screens = self.screens(window, cx);
        let first_audio = FIRST_SCREEN_ROW + screens.len();
        let selected = settings.selected.min(first_audio);
        let checkbox = |on: bool, enabled: bool| {
            div()
                .size(px(14.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(if on { Theme::accent() } else { Theme::border() })
                .when(!enabled, |el| el.opacity(0.4))
                .when(on, |el| el.bg(Theme::accent()).child(icon(Icon::Check, px(12.), Theme::editor_bg())))
        };

        // Upscaling: row 0.
        let upscaler = upscale::available();
        let upscale_row = div()
            .id("upscale-choice")
            .h(px(32.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(4.))
            .text_sm()
            .when(selected == 0, |el| el.bg(Theme::selected()))
            .when(upscaler.is_some(), |el| el.hover(|s| s.bg(Theme::hover())).cursor_pointer())
            .on_click(cx.listener(|this, _, window, cx| this.settings_toggle(0, window, cx)))
            .child(checkbox(self.upscaling && upscaler.is_some(), upscaler.is_some()))
            .child(div().flex_1().min_w_0().truncate().text_color(Theme::text()).child("Upscale video"));
        let upscale_detail = match upscaler {
            Some(name) => format!("Upscales using {name}"),
            None => "No supported GPU".into(),
        };
        let upscale_now = self.upscaling.then(|| self.player.as_ref().map(|p| Self::upscale_label(&p.upscale_status()))).flatten();

        // Stills: row 1.
        let secs = self.still_seconds;
        let typed = settings.typed.clone();
        let still_row = div()
            .id("still-choice")
            .h(px(32.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(4.))
            .text_sm()
            .when(selected == 1, |el| el.bg(Theme::selected()))
            .child(div().flex_1().min_w_0().truncate().text_color(Theme::text()).child("Show stills for"))
            .child(icon_button("still-less", Icon::Minus, false, cx.listener(move |this, _, _, cx| {
                this.settings_select(1);
                this.set_still_seconds(step_still(secs, -1), cx)
            })))
            .child(
                // Click to type a number of seconds.
                div()
                    .id("still-value")
                    .w(px(96.))
                    .h(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(if typed.is_some() { Theme::accent() } else { Theme::border_variant() })
                    .cursor_text()
                    .text_color(Theme::text())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(s) = this.settings.as_mut() {
                            s.selected = 1;
                            s.typed = Some(String::new());
                        }
                        cx.notify();
                    }))
                    .map(|el| match &typed {
                        Some(t) => el.child(div().flex().items_center().child(t.clone()).child(caret()).child(" s")),
                        None => el.child(still_label(secs)),
                    }),
            )
            .child(icon_button("still-more", Icon::Plus, false, cx.listener(move |this, _, _, cx| {
                this.settings_select(1);
                this.set_still_seconds(step_still(secs, 1), cx)
            })));

        // Dissolve: row 2. The box switches it, the buttons set its length.
        let dissolve_secs = self.dissolve_secs;
        let dissolve_row = div()
            .id("dissolve-choice")
            .debug_selector(|| "dissolve-choice".into())
            .h(px(32.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(4.))
            .text_sm()
            .when(selected == DISSOLVE_ROW, |el| el.bg(Theme::selected()))
            .hover(|s| s.bg(Theme::hover()))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| this.settings_toggle(DISSOLVE_ROW, window, cx)))
            .child(checkbox(self.dissolve, true))
            .child(div().flex_1().min_w_0().truncate().text_color(Theme::text()).child("Dissolve between entries"))
            .child(div().debug_selector(|| "dissolve-less".into()).child(icon_button("dissolve-less", Icon::Minus, false, cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.settings_select(DISSOLVE_ROW);
                this.set_dissolve_secs(dissolve::step_dissolve(dissolve_secs, -1), cx)
            }))))
            .child(
                div()
                    .w(px(96.))
                    .h(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(Theme::border_variant())
                    .text_color(if self.dissolve { Theme::text() } else { Theme::text_placeholder() })
                    .child(dissolve::dissolve_label(dissolve_secs)),
            )
            .child(icon_button("dissolve-more", Icon::Plus, false, cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.settings_select(DISSOLVE_ROW);
                this.set_dissolve_secs(dissolve::step_dissolve(dissolve_secs, 1), cx)
            })));

        let row = |ix: usize, screen: &Screen| {
            let ix = ix + FIRST_SCREEN_ROW;
            let on = self.output_on(&screen.uuid);
            div()
                .id(("output-choice", ix))
                .h(px(32.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(px(4.))
                .text_sm()
                .when(ix == selected, |el| el.bg(Theme::selected()))
                .hover(|s| s.bg(Theme::hover()))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| this.settings_toggle(ix, window, cx)))
                .child(checkbox(on, true))
                .child(div().flex_1().min_w_0().truncate().text_color(Theme::text()).child(screen.label.clone()))
                .child(div().flex_none().text_xs().text_color(Theme::text_placeholder()).child(self.screen_detail(screen)))
        };
        let choices = screens.iter().enumerate().map(|(ix, s)| row(ix, s)).collect::<Vec<_>>();

        // Audio output: one choice, so a dropdown.
        let audio_options = self.audio_choices();
        let current = self.audio_choice_ix();
        let chosen_connected = self.audio_output.as_ref().is_none_or(|_| current > 0);
        let audio_button = div()
            .id("audio-choice")
            .debug_selector(|| "audio-choice".into())
            .h(px(32.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(4.))
            .border_1()
            .border_color(if settings.audio_menu.is_some() { Theme::accent() } else { Theme::border_variant() })
            .text_sm()
            .when(selected == first_audio, |el| el.bg(Theme::selected()))
            .hover(|s| s.bg(Theme::hover()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.settings_toggle(first_audio, window, cx)))
            .child(div().flex_1().min_w_0().truncate().text_color(Theme::text()).child(audio_options[current].1.clone()))
            .child(icon(Icon::ChevronDown, px(14.), Theme::text_muted()));
        let audio_menu = settings.audio_menu.map(|highlighted| {
            let items = audio_options.iter().enumerate().map(|(ix, (_, name))| {
                div()
                    .id(("audio-item", ix))
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(4.))
                    .text_sm()
                    .text_color(Theme::text())
                    .when(ix == highlighted, |el| el.bg(Theme::selected()))
                    .hover(|s| s.bg(Theme::hover()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.pick_audio(ix, window, cx)))
                    .child(div().size(px(14.)).flex_none().when(ix == current, |el| el.child(icon(Icon::Check, px(14.), Theme::accent()))))
                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
            });
            // Just under the button, over whatever is below it.
            div().absolute().top(relative(1.)).left_0().child(deferred(
                anchored().snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("audio-menu")
                        .debug_selector(|| "audio-menu".into())
                        .occlude()
                        .mt(px(4.))
                        .w(px(496.))
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .bg(Theme::elevated_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(6.))
                        .shadow_lg()
                        // A click elsewhere closes the list and does
                        // nothing else.
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            if let Some(s) = this.settings.as_mut() {
                                s.audio_menu = None;
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .children(items),
                ),
            ))
        });

        let panel = div()
            .id("settings")
            .key_context(SETTINGS_CONTEXT)
            .track_focus(&self.settings_focus)
            .on_action(cx.listener(|this, _: &PickerUp, window, cx| this.settings_move(-1, window, cx)))
            .on_action(cx.listener(|this, _: &PickerDown, window, cx| this.settings_move(1, window, cx)))
            .on_action(cx.listener(|this, _: &PickerConfirm, window, cx| {
                let ix = this.settings.as_ref().map_or(0, |s| s.selected);
                this.settings_toggle(ix, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PickerDismiss, window, cx| {
                // Escape closes the audio output list first.
                match this.settings.as_mut().filter(|s| s.audio_menu.is_some()) {
                    Some(s) => {
                        s.audio_menu = None;
                        cx.notify();
                    }
                    None => this.close_settings(window, cx),
                }
            }))
            .on_key_down(cx.listener(Self::settings_key))
            .on_action(cx.listener(|this, _: &SettingsLess, _, cx| this.settings_adjust(-1, cx)))
            .on_action(cx.listener(|this, _: &SettingsMore, _, cx| this.settings_adjust(1, cx)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .occlude()
            .mt(px(72.))
            .w(px(520.))
            .flex()
            .flex_col()
            .bg(Theme::elevated_bg())
            .border_1()
            .border_color(Theme::border())
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .h(px(40.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(Theme::border_variant())
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Settings"))
                    .child(icon_button("settings-close", Icon::Close, false, cx.listener(|this, _, window, cx| this.close_settings(window, cx)))),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child("Upscaling"))
                    .child(upscale_row)
                    .child(div().px(px(8.)).text_xs().text_color(Theme::text_muted()).child(upscale_detail))
                    .when_some(upscale_now, |el, now| el.child(div().px(px(8.)).text_xs().text_color(Theme::text_placeholder()).child(format!("Now: {now}")))),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child("Stills"))
                    .child(still_row),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child("Transitions"))
                    .child(dissolve_row),
            )
            .child(
                div()
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child("Outputs"))
                    .child(
                        div()
                            .pb(px(6.))
                            .text_xs()
                            .text_color(Theme::text_muted())
                            .child("Show video full screen on these displays."),
                    )
                    .children(choices)
                    .when(!self.outputs_on && !self.output_setting.is_empty(), |el| {
                        el.child(
                            div()
                                .px(px(8.))
                                .pt(px(4.))
                                .text_xs()
                                .text_color(Theme::warning())
                                .child(format!("Outputs are off ({})", window.keystroke_text_for(&ToggleOutputs))),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(12.))
                    .pb(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child("Audio output"))
                    .child(div().relative().child(audio_button).children(audio_menu))
                    .when(!chosen_connected, |el| {
                        el.child(
                            div()
                                .px(px(8.))
                                .pt(px(4.))
                                .text_xs()
                                .text_color(Theme::warning())
                                .child("Chosen output not connected. Using the system default."),
                        )
                    }),
            );

        Some(
            div()
                .id("settings-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                // A click outside only dismisses: it does not also reach
                // what is underneath (such as the gear that opened this).
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_settings(window, cx)))
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chosen(uuids: &[&str]) -> Vec<String> {
        uuids.iter().map(|u| u.to_string()).collect()
    }

    #[test]
    fn still_durations_are_whole_seconds_typed_or_stepped() {
        assert_eq!((step_still(10, 1), step_still(10, -1)), (11, 9));
        assert_eq!(step_still(0, -1), 0, "stops at until skipped");
        assert_eq!(step_still(MAX_STILL_SECONDS, 1), MAX_STILL_SECONDS, "stops at an hour");
        let typed = ["1", "2", "7"].iter().fold(String::new(), |t, k| type_still(&t, k).unwrap());
        assert_eq!(typed, "127");
        assert_eq!(type_still("127", "backspace").as_deref(), Some("12"));
        assert_eq!(type_still("0", "5").as_deref(), Some("5"), "a leading zero is replaced");
        assert_eq!(type_still("999", "9").as_deref(), Some("3600"), "capped at an hour");
        assert_eq!(type_still("12", "a"), None, "not a digit");
        assert_eq!((still_label(0), still_label(45)), ("Until skipped".to_string(), "45 s".to_string()));
    }

    #[test]
    fn each_output_follows_its_display_in_and_out() {
        let none: (Vec<String>, Vec<String>) = (vec![], vec![]);
        assert_eq!(plan(&[], &["a", "b"], &[]), none, "nothing chosen");
        assert_eq!(plan(&chosen(&["a", "b"]), &["a", "b"], &[]), (chosen(&["a", "b"]), vec![]), "two at once");
        assert_eq!(plan(&chosen(&["a", "b"]), &["a", "b"], &["a", "b"]), none, "both showing");
        assert_eq!(plan(&chosen(&["a", "b"]), &["a"], &["a", "b"]), (vec![], chosen(&["b"])), "b unplugged, a stays");
        assert_eq!(plan(&chosen(&["a", "b"]), &["a"], &["a"]), none, "waiting for b");
        assert_eq!(plan(&chosen(&["a", "b"]), &["a", "b"], &["a"]), (chosen(&["b"]), vec![]), "b plugged back in");
        assert_eq!(plan(&chosen(&["a"]), &["a", "b"], &["a", "b"]), (vec![], chosen(&["b"])), "b turned off");
        assert_eq!(plan(&[], &["a"], &["a"]), (vec![], chosen(&["a"])), "all turned off");
    }
}
