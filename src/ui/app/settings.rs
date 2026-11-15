//! The settings screen, and the external outputs it configures.
//!
//! Outputs are chosen per display in the settings, by each display's
//! stable id, and any number can be on at once, all showing the same
//! picture. The status bar button switches them all on and off. While a chosen
//! display is connected the picture is full screen on it; when it is
//! unplugged its window closes, and it comes back when the display does,
//! without anyone touching the controls.

use super::*;

pub(super) const SETTINGS_CONTEXT: &str = "Settings";

/// How often the connected displays are checked, so an output display
/// plugged in or out is noticed.
const DISPLAY_POLL: Duration = Duration::from_millis(500);

pub(super) struct SettingsScreen {
    pub(super) selected: usize,
}

/// A connected display, as the settings and the footer menu name it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Screen {
    /// Stable across reconnects and restarts; what the setting stores.
    pub(super) uuid: String,
    pub(super) label: String,
    pub(super) size: String,
    /// The controls are on this display.
    pub(super) here: bool,
}

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
    /// What the output windows show: the video frame, else the cover art.
    pub(in crate::ui) fn picture(&self) -> Option<Arc<RenderImage>> {
        self.frame.clone().or_else(|| self.cover.clone())
    }

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

    /// The detail shown next to a display in the settings and footer menu.
    pub(super) fn screen_detail(&self, s: &Screen) -> String {
        if s.here { format!("{}, covers this window", s.size) } else { s.size.clone() }
    }

    // ---- settings screen ----------------------------------------------------

    pub(super) fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(window, cx);
            return;
        }
        self.picker = None;
        self.settings = Some(SettingsScreen { selected: 0 });
        window.focus(&self.settings_focus);
        cx.notify();
    }

    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Settings rows: the upscaling switch, then one per display.
    fn settings_move(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let len = 1 + self.screens(window, cx).len() as isize;
        if let Some(s) = self.settings.as_mut()
            && len > 0
        {
            s.selected = (s.selected as isize + by).rem_euclid(len) as usize;
            cx.notify();
        }
    }

    fn settings_toggle(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.settings.as_mut() {
            s.selected = ix;
        }
        if ix == 0 {
            // Nothing to switch on without a GPU that can do it.
            if upscale::available().is_some() {
                self.set_upscaling(!self.upscaling, cx);
            }
            return;
        }
        let Some(screen) = self.screens(window, cx).into_iter().nth(ix - 1) else { return };
        self.toggle_output(screen.uuid, window, cx);
    }

    pub(super) fn render_settings(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let settings = self.settings.as_ref()?;
        let screens = self.screens(window, cx);
        let selected = settings.selected.min(screens.len());
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

        let row = |ix: usize, screen: &Screen| {
            let ix = ix + 1;
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
            .on_action(cx.listener(|this, _: &PickerDismiss, window, cx| this.close_settings(window, cx)))
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
