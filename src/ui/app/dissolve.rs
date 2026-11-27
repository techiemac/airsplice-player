//! The optional dissolve between playlist entries.
//!
//! When it is on, moving to another entry (autoplay, or picking one) does
//! not close the file on screen: it keeps playing underneath while the new
//! one fades in over it, and their sound crossfades. Autoplay starts the
//! next entry one dissolve before the current one ends, so the two overlap
//! instead of the new one fading in over a stopped picture. Tab switches
//! stay cuts.
//!
//! An entry can set its own length (from an `.airsplice` playlist file),
//! 0 cutting to it; that holds whether or not the setting is on.

use gpui::{Corner, anchored, deferred, relative};

use super::*;

pub(super) const MIN_DISSOLVE: f32 = 0.5;
pub(super) const MAX_DISSOLVE: f32 = 5.0;
const DISSOLVE_STEP: f32 = 0.5;

/// `secs` moved by `by` steps of half a second, within the limits.
pub(super) fn step_dissolve(secs: f32, by: i64) -> f32 {
    let steps = (secs / DISSOLVE_STEP).round() as i64 + by;
    (steps as f32 * DISSOLVE_STEP).clamp(MIN_DISSOLVE, MAX_DISSOLVE)
}

/// An entry's own dissolve length moved by `by` steps. A length set longer
/// than the steps go (in a file) is not shortened by a step up.
pub(super) fn step_entry_dissolve(secs: f32, by: i64) -> f32 {
    if by > 0 && secs >= MAX_DISSOLVE { secs } else { step_dissolve(secs, by) }
}

pub(super) fn dissolve_label(secs: f32) -> String {
    if secs.fract() == 0.0 { format!("{secs:.0} s") } else { format!("{secs:.1} s") }
}

/// How long the dissolve waits for the new entry's first picture (a large
/// image can take a moment) before it starts anyway.
const WAIT_FOR_PICTURE: Duration = Duration::from_secs(1);

/// The entry being dissolved away from.
pub(super) struct Outgoing {
    pub(super) loaded: Loaded,
    /// The dissolve's length, in seconds.
    secs: f32,
    /// When the fade began: once the new entry has a picture to fade to.
    started: Option<Instant>,
    since: Instant,
}

/// Sound levels (outgoing, incoming) at `progress`: an equal-power
/// crossfade, so the overall loudness holds steady through it.
fn gains(progress: f32) -> (f32, f32) {
    let angle = progress.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

impl MediaPlayer {
    /// Seconds moving into playlist entry `ix` dissolves over: the entry's
    /// own length, else the setting's; `None` when it cuts.
    pub(super) fn dissolve_into(&self, ix: usize) -> Option<f32> {
        let secs = match self.entry_dissolve.get(ix).copied().flatten() {
            Some(own) => own,
            None if self.dissolve => self.dissolve_secs,
            None => 0.0,
        };
        (secs > 0.0).then_some(secs)
    }

    /// Take what is on screen to dissolve from into entry `ix`, with the
    /// dissolve's length, when that dissolves and something is open. A
    /// dissolve already under way ends first.
    pub(super) fn dissolve_from(&mut self, ix: usize, window: &mut Window) -> Option<(Loaded, f32)> {
        self.end_dissolve(window);
        let secs = self.dissolve_into(ix)?;
        self.player.as_ref()?;
        Some((self.take_installed(), secs))
    }

    /// Start dissolving from `from` into what is now on screen. If the new
    /// entry could not be opened, its error shows at once instead.
    pub(super) fn begin_dissolve(&mut self, (from, secs): (Loaded, f32), window: &mut Window) {
        if self.player.is_none() {
            from.release(window);
            return;
        }
        self.outgoing = Some(Outgoing { loaded: from, secs, started: None, since: Instant::now() });
    }

    /// Finish any dissolve now: the outgoing file closes and the new one
    /// plays at full volume.
    pub(super) fn end_dissolve(&mut self, window: &mut Window) {
        if let Some(out) = self.outgoing.take() {
            out.loaded.release(window);
            if let Some(p) = &self.player {
                p.set_volume(self.volume, self.muted);
            }
        }
    }

    /// The volume a file starts at: silent when it is about to fade in.
    pub(super) fn entry_volume(&self) -> f32 {
        if self.fading_in { 0.0 } else { self.volume }
    }

    /// How far the dissolve has got, 0 to 1; `None` when there is none.
    pub(super) fn dissolve_progress(&self) -> Option<f32> {
        let out = self.outgoing.as_ref()?;
        Some(out.started.map_or(0.0, |t| (t.elapsed().as_secs_f32() / out.secs).min(1.0)))
    }

    /// Called every tick: moves the outgoing picture and both volumes on,
    /// and ends the dissolve when it is complete. True while one is under
    /// way, so the window redraws.
    pub(super) fn tick_dissolve(&mut self, window: &mut Window) -> bool {
        let incoming_ready = match &self.player {
            Some(p) => self.frame.is_some() || !p.shows_picture(),
            None => true,
        };
        let Some(out) = self.outgoing.as_mut() else { return false };
        if let Some(frame) = out.loaded.player.as_ref().and_then(Player::take_frame)
            && let Some(old) = out.loaded.frame.replace(to_render_image(frame.width, frame.height, frame.bgra))
        {
            let _ = window.drop_image(old);
        }
        if out.started.is_none() && (incoming_ready || out.since.elapsed() >= WAIT_FOR_PICTURE) {
            out.started = Some(Instant::now());
        }
        let progress = self.dissolve_progress().unwrap_or(1.0);
        if progress >= 1.0 {
            self.end_dissolve(window);
            return true;
        }
        let (gone, coming) = gains(progress);
        if let Some(out) = self.outgoing.as_ref().and_then(|o| o.loaded.player.as_ref()) {
            out.set_volume(self.volume * gone, self.muted);
        }
        if let Some(p) = &self.player {
            p.set_volume(self.volume * coming, self.muted);
        }
        true
    }

    /// Whether autoplay should move on now, one dissolve (into the next
    /// entry) before the end of the entry on screen. Entries shorter than
    /// two dissolves play out.
    pub(super) fn dissolve_due(&self) -> bool {
        let Some(p) = &self.player else { return false };
        let Some(secs) = self.upcoming().and_then(|ix| self.dissolve_into(ix)) else { return false };
        let secs = secs as f64;
        self.outgoing.is_none()
            && !self.ended
            && !p.is_paused()
            && p.info.duration.is_some_and(|d| d >= 2.0 * secs && p.position() >= d - secs)
    }

    /// The pictures to draw, bottom first, each with its opacity: during a
    /// dissolve, the outgoing picture under the incoming one. With
    /// `covers`, cover art counts as a picture (the output windows show it
    /// full screen; the main window shows it beside the title).
    pub(in crate::ui) fn layers(&self, covers: bool) -> Vec<(Arc<RenderImage>, f32)> {
        let pick = |frame: &Option<Arc<RenderImage>>, cover: &Option<Arc<RenderImage>>| {
            frame.clone().or_else(|| if covers { cover.clone() } else { None })
        };
        let incoming = pick(&self.frame, &self.cover);
        let outgoing = self.outgoing.as_ref().and_then(|o| pick(&o.loaded.frame, &o.loaded.cover));
        match (outgoing, incoming, self.dissolve_progress()) {
            // Not started: the new entry's picture is not up yet.
            (Some(out), _, Some(p)) if p <= 0.0 => vec![(out, 1.0)],
            (Some(out), Some(inc), Some(p)) => vec![(out, 1.0), (inc, p)],
            (_, inc, _) => inc.into_iter().map(|i| (i, 1.0)).collect(),
        }
    }

    /// Set how the entry on screen comes in: its own length (0 cuts),
    /// or `None` to follow the setting. Remembered, and saved in playlist
    /// files.
    pub(super) fn set_current_dissolve(&mut self, own: Option<f32>, cx: &mut Context<Self>) {
        if let Some(d) = self.current.and_then(|ix| self.entry_dissolve.get_mut(ix)) {
            *d = own;
            cx.notify();
        }
    }

    /// Lengthen or shorten the dissolve into the entry on screen.
    fn step_current_dissolve(&mut self, by: i64, cx: &mut Context<Self>) {
        let Some(ix) = self.current else { return };
        let now = self.dissolve_into(ix).unwrap_or(self.dissolve_secs);
        self.set_current_dissolve(Some(step_entry_dissolve(now, by)), cx);
    }

    /// Beside the repeat button: how the entry on screen comes in, and a
    /// menu to change it. Dim while it follows the setting.
    pub(super) fn render_transition_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ix = self.current?;
        let own = self.entry_dissolve.get(ix).copied().flatten();
        let secs = self.dissolve_into(ix);
        let color = if own.is_some() { Theme::accent() } else { Theme::text_muted() };
        let button = div()
            .id("transition")
            .debug_selector(|| "transition".into())
            .h(px(24.))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(4.))
            .rounded(px(4.))
            .text_xs()
            .text_color(color)
            .hover(|s| s.bg(Theme::hover()))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.transition_menu = !this.transition_menu;
                cx.notify();
            }))
            .child(icon(if secs.is_some() { Icon::Dissolve } else { Icon::Cut }, px(14.), color))
            .child(match secs {
                Some(s) => super::playlist_file::entry_dissolve_label(s),
                None => "Cut".into(),
            });
        Some(div().relative().child(button).children(self.transition_menu.then(|| self.render_transition_menu(own, cx))).into_any_element())
    }

    fn render_transition_menu(&self, own: Option<f32>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: SharedString, checked: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(28.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(4.))
                .text_sm()
                .text_color(Theme::text())
                .hover(|s| s.bg(Theme::hover()))
                .cursor_pointer()
                .child(div().size(px(14.)).flex_none().when(checked, |el| el.child(icon(Icon::Check, px(14.), Theme::accent()))))
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        let dissolving = own.is_some_and(|s| s > 0.0);
        let length = own.filter(|s| *s > 0.0).unwrap_or(self.dissolve_secs);
        let step = |id: &'static str, glyph: Icon, by: i64| {
            div().debug_selector(move || id.into()).child(icon_button(id, glyph, false, cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.step_current_dissolve(by, cx)
            })))
        };
        let less = step("transition-less", Icon::Minus, -1);
        let more = step("transition-more", Icon::Plus, 1);
        let setting: SharedString = if self.dissolve {
            format!("Use setting ({})", dissolve_label(self.dissolve_secs)).into()
        } else {
            "Use setting (cut)".into()
        };
        let pick = |own: Option<f32>| {
            move |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.set_current_dissolve(own, cx);
                this.transition_menu = false;
            }
        };
        // Above the button: the controls are at the bottom of the window.
        div()
            .absolute()
            .bottom(relative(1.))
            .left_0()
            .child(deferred(
                anchored().anchor(Corner::BottomLeft).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("transition-menu")
                        .occlude()
                        .mb(px(4.))
                        .w(px(220.))
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .bg(Theme::elevated_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(6.))
                        .shadow_lg()
                        // A click elsewhere closes it and does nothing else.
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.transition_menu = false;
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child(item("transition-cut", "Cut".into(), own == Some(0.0)).on_click(cx.listener(pick(Some(0.0)))))
                        .child(
                            item("transition-dissolve", "Dissolve".into(), dissolving)
                                .on_click(cx.listener(pick(Some(length))))
                                .child(less)
                                .child(
                                    div()
                                        .w(px(40.))
                                        .flex()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(if dissolving { Theme::text() } else { Theme::text_placeholder() })
                                        .child(super::playlist_file::entry_dissolve_label(length)),
                                )
                                .child(more),
                        )
                        .child(div().my(px(4.)).h(px(1.)).bg(Theme::border()))
                        .child(item("transition-setting", setting, own.is_none()).on_click(cx.listener(pick(None)))),
                ),
            ))
            .into_any_element()
    }

    /// Turn the dissolve on or off for entries without their own length.
    /// Remembered.
    pub(super) fn set_dissolve(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.dissolve = on;
        if !on {
            self.end_dissolve(window);
        }
        cx.notify();
    }

    pub(super) fn set_dissolve_secs(&mut self, secs: f32, cx: &mut Context<Self>) {
        self.dissolve_secs = secs.clamp(MIN_DISSOLVE, MAX_DISSOLVE);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_step_by_half_seconds_within_the_limits() {
        assert_eq!(step_dissolve(1.0, 1), 1.5);
        assert_eq!(step_dissolve(1.0, -1), 0.5);
        assert_eq!(step_dissolve(0.5, -1), 0.5);
        assert_eq!(step_dissolve(5.0, 1), 5.0);
        // A stored value off the grid lands back on it.
        assert_eq!(step_dissolve(1.3, 1), 2.0);
        assert_eq!((dissolve_label(1.0), dissolve_label(1.5)), ("1 s".to_string(), "1.5 s".to_string()));
    }

    #[test]
    fn an_entrys_own_length_steps_within_the_limits_but_keeps_a_longer_one() {
        assert_eq!(step_entry_dissolve(1.0, 1), 1.5);
        assert_eq!(step_entry_dissolve(0.5, -1), 0.5);
        assert_eq!(step_entry_dissolve(5.0, 1), 5.0);
        // A longer length from a file stays until stepped down.
        assert_eq!(step_entry_dissolve(8.0, 1), 8.0);
        assert_eq!(step_entry_dissolve(8.0, -1), 5.0);
    }

    #[test]
    fn the_crossfade_keeps_the_loudness_steady() {
        assert_eq!(gains(0.0), (1.0, 0.0));
        let (a, b) = gains(1.0);
        assert!(a.abs() < 1e-6 && (b - 1.0).abs() < 1e-6);
        for p in [0.1, 0.25, 0.5, 0.9] {
            let (a, b) = gains(p);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "power at {p}");
        }
    }
}
