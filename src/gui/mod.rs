//! The GUI front-end.
//!
//! `streampipe` with no arguments opens this. It owns the eframe event loop on
//! the main thread and drives the async iroh engine on a background tokio
//! runtime over channels. See [`state`] for the threading contract.

mod broadcast;
mod config;
mod log;
mod options;
mod state;
mod watch;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dumbpipe::webrtc::{Player, StreamStats};
use eframe::egui::{self, Ui};
use tokio::{runtime::Handle, sync::mpsc};

use self::{
    config::Config,
    state::{Mode, SharedState},
};

/// The repaint interval used while a session is live or a toast is showing, so
/// logs and stats refresh without a per-event repaint storm.
const REPAINT: Duration = Duration::from_millis(250);
/// How long a "Copied" toast stays visible.
const TOAST: Duration = Duration::from_millis(1500);
/// How long a copy box's click flash brightens its fill and glyph. Shorter than
/// [`TOAST`] so the repaint guard stays armed through the whole flash.
const FLASH: Duration = Duration::from_millis(450);
/// The "live/success" accent: broadcasting status, the copy toast, and viewers.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);

/// The two main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// Watch a stream from a ticket.
    Watch,
    /// Start broadcasting to OBS over WHIP.
    Broadcast,
}

/// The application.
pub struct App {
    /// Handle to the background tokio runtime that runs the engine.
    handle: Handle,
    /// Cross-thread shared state.
    state: Arc<Mutex<SharedState>>,
    /// The persisted configuration, edited live and saved on change and quit.
    config: Config,
    /// The log ring fed by the tee writer.
    log: log::LogRing,
    /// The stats channel sender, cloned to each broadcast.
    stats_tx: mpsc::UnboundedSender<StreamStats>,
    /// The selected tab.
    tab: Tab,
    /// Whether the options panel is open.
    show_options: bool,
    /// Whether the bottom log panel is expanded.
    log_open: bool,
    /// The ticket text on the Watch tab.
    watch_ticket: String,
    /// The player selected on the Watch tab.
    watch_player: Player,
    /// The buffer field on the Watch tab, in milliseconds (empty = auto).
    watch_buffer: String,
    /// The last copied box and when, for the transient toast.
    copied: Option<(&'static str, Instant)>,
    /// A transient action-complete toast (e.g. "Relay refreshed ✓") and when it was
    /// raised, shown by [`App::toast_overlay`] in the same pill as a copy toast.
    toast: Option<(String, Instant)>,
    /// Clock origin for the viewers pulse, so it animates smoothly while live.
    pulse: Instant,
    /// Whether the next window-focused frame should autofocus the active tab's
    /// primary control (the ticket box on Watch, the start button on Broadcast).
    /// Set on open and on every tab switch; cleared the first time a window that
    /// actually has keyboard focus consumes it, so it never steals focus from a
    /// widget the user has already chosen.
    focus_primary: bool,
}

impl App {
    /// Build the app from the runtime, config and channels.
    fn new(
        handle: Handle,
        config: Config,
        state: Arc<Mutex<SharedState>>,
        log: log::LogRing,
        stats_tx: mpsc::UnboundedSender<StreamStats>,
    ) -> Self {
        let watch_player = config.default_player();
        let watch_buffer = config
            .buffer_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        App {
            handle,
            state,
            watch_ticket: config.last_ticket.clone(),
            config,
            log,
            stats_tx,
            tab: Tab::Watch,
            show_options: false,
            log_open: false,
            watch_player,
            watch_buffer,
            copied: None,
            toast: None,
            pulse: Instant::now(),
            focus_primary: true,
        }
    }

    /// The top menu bar with the status and the Options entry.
    fn menu_bar(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        // Computed before the closure: the closure mutably borrows `self` for the
        // Options toggle, so the pulse clock is read here.
        let secs = self.pulse.elapsed().as_secs_f32();
        egui::menu::MenuBar::new().ui(ui, |ui| {
            // Open the settings panel directly — no intermediate dropdown.
            if ui.button("Settings").clicked() {
                self.show_options = !self.show_options;
            }
            ui.separator();
            let base = match snap.mode {
                Mode::Idle => egui::Color32::GRAY,
                Mode::Starting => egui::Color32::YELLOW,
                Mode::Broadcasting => ACCENT,
                Mode::Watching => egui::Color32::from_rgb(90, 160, 255),
            };
            // The dot breathes only while live; `Starting` stays calm yellow and
            // `Idle` flat gray, so a steady dot never reads as a running session.
            let pulsing = matches!(snap.mode, Mode::Broadcasting | Mode::Watching);
            let dot = if pulsing {
                base.gamma_multiply(pulse_factor(secs))
            } else {
                base
            };
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.colored_label(dot, "●");
            ui.colored_label(base, snap.mode.status());
            if let Some(err) = &snap.error {
                ui.colored_label(egui::Color32::from_rgb(220, 90, 90), truncate(err, 60));
            }
        });
    }

    /// The tab strip.
    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.heading("streampipe");
        ui.add_space(6.0);
        // Two equal-width tabs that fill the whole strip; the selectable label is
        // sized to its half so the entire tab box (not just the text) is the click
        // target.
        let half = ui.available_width() / 2.0;
        let tab_h = 32.0;
        let (watch, bc) = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Style both states explicitly (rather than relying on `.selected()`,
                // which only themes the active tab): the selected tab is a lighter
                // surface with full-strength text (plus the accent underline below),
                // while unselected tabs recede to a darker surface and dimmed text,
                // keeping focus on the primary Start Broadcasting action.
                let make = |ui: &mut Ui, label: &str, selected: bool| {
                    let fill = if selected {
                        egui::Color32::from_white_alpha(26)
                    } else {
                        egui::Color32::from_black_alpha(70)
                    };
                    let stroke = egui::Stroke::new(
                        1.0,
                        if selected {
                            egui::Color32::from_white_alpha(45)
                        } else {
                            egui::Color32::from_white_alpha(10)
                        },
                    );
                    let text = egui::RichText::new(label).color(if selected {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::GRAY
                    });
                    ui.add_sized([half, tab_h], egui::Button::new(text).fill(fill).stroke(stroke))
                };
                let w = make(ui, "Watch Stream", self.tab == Tab::Watch);
                let b = make(ui, "Broadcast", self.tab == Tab::Broadcast);
                if w.clicked() {
                    self.tab = Tab::Watch;
                    self.focus_primary = true;
                }
                if b.clicked() {
                    self.tab = Tab::Broadcast;
                    self.focus_primary = true;
                }
                (w, b)
            })
            .inner;
        // A persistent accent underline marks the active tab, so selection is
        // legible even when the selectable_label background is subtle. A slim,
        // rounded bar inset from each tab edge reads as deliberate rather than a
        // heavy full-width block.
        let active_rect = if self.tab == Tab::Watch {
            watch.rect
        } else {
            bc.rect
        };
        let underline = egui::Rect::from_min_max(
            egui::pos2(active_rect.min.x + 5.0, active_rect.max.y - 2.0),
            egui::pos2(active_rect.max.x - 5.0, active_rect.max.y),
        );
        ui.painter().rect_filled(underline, egui::CornerRadius::same(1), ACCENT);
        ui.separator();
    }

    /// The collapsible log panel along the bottom.
    fn log_panel(&mut self, ui: &mut Ui) {
        let resp = egui::Panel::bottom("log")
            .resizable(true)
            .default_size(160.0)
            .show_collapsible(ui, &mut self.log_open, |ui| {
                let lines = log::snapshot(&self.log);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if lines.is_empty() {
                            ui.weak("no log output yet");
                        }
                        for line in lines {
                            ui.label(egui::RichText::new(line).monospace().size(12.0));
                        }
                    });
            });
        // Repaint while expanded so streaming logs appear without a storm.
        if resp.is_some() {
            ui.ctx().request_repaint_after(REPAINT);
        }
    }

    /// The floating status pill ("Copied ✓", "Relay refreshed ✓", …), anchored
    /// bottom-centre.
    ///
    /// Drawn in its own foreground [`egui::Area`] rather than inline, so confirming a
    /// copy or a relay/ticket action never nudges the WHIP URL box, the ticket box or
    /// the stats panel. A copy toast (`copied`) takes precedence over an action toast
    /// (`toast`) when both are momentarily live. The repaint guard in [`App::draw`]
    /// keeps frames coming while either is up and expires them, so the fade completes
    /// exactly as repaints stop and no half-faded pill is left on screen.
    fn toast_overlay(&self, ui: &Ui) {
        // `App::draw` already expired both for this frame, so if either is still set
        // it is live; the expiry threshold lives in one place there.
        let (text, at) = match (&self.copied, &self.toast) {
            (Some((_, at)), _) => ("Copied ✓", *at),
            (None, Some((msg, at))) => (msg.as_str(), *at),
            (None, None) => return,
        };
        let elapsed = at.elapsed();
        // Fade the whole pill out over its remaining lifetime.
        let alpha = (1.0 - elapsed.as_secs_f32() / TOAST.as_secs_f32()).clamp(0.0, 1.0);
        let frame = egui::Frame::popup(ui.style())
            .fill(egui::Color32::from_rgb(30, 42, 36).gamma_multiply(alpha))
            .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(alpha)))
            .inner_margin(egui::Margin::symmetric(14, 6));
        egui::Area::new(egui::Id::new("copy_toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ui.ctx(), |ui| {
                frame.show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(text)
                            .strong()
                            .color(ACCENT.gamma_multiply(alpha)),
                    );
                });
            });
    }
}

impl App {
    /// Lay out the whole window.
    ///
    /// Split out of [`eframe::App::ui`], which only forwards here, so the real
    /// frame can be driven headlessly by tests without a window or GL context.
    fn draw(&mut self, ui: &mut Ui) {
        // Expire the copy toast before deciding on the next frame. A click later in
        // this same frame re-arms `copied` with a fresh `Instant`, so expiring here
        // never races a click.
        self.copied = expire_copied(self.copied, TOAST);
        self.toast = self.toast.take().filter(|(_, at)| at.elapsed() < TOAST);

        let (mode, refreshing) = {
            let g = self.state.lock().unwrap();
            (g.mode, g.refreshing)
        };
        // Pick up any completion notice a background task left (relay refresh or
        // ticket reset) and raise it as a toast. Taken under the same brief lock that
        // reads mode/refreshing.
        let notice = self.state.lock().unwrap().notice.take();
        if let Some(msg) = notice {
            self.toast = Some((msg, Instant::now()));
        }
        // Persist a ticket minted while idle (via Refresh Relay / Reset Ticket) so the
        // saved "current ticket" survives restarts. Guarded to a real change so we
        // never rewrite config.json every frame. The live broadcast path persists its
        // own ticket in the broadcast tab, so this only runs when idle.
        if !mode.active() {
            let g = self.state.lock().unwrap();
            if let Some(t) = g.ticket.as_ref() {
                if Some(t) != self.config.broadcast_ticket.as_ref() {
                    self.config.broadcast_ticket = Some(t.clone());
                    drop(g);
                    self.config.save();
                }
            }
        }
        // Repaint while live, while a toast fades, or while a refresh is in flight so
        // the button re-enables and the new ticket appears without user input.
        if mode.active() || refreshing || self.copied.is_some() || self.toast.is_some() {
            ui.ctx().request_repaint_after(REPAINT);
        }

        // On close, abort any live session (frees the WHIP port, kills the
        // player) and persist config. The endpoint and runtime are dropped in
        // `run` after `run_native` returns.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            state::stop(&self.state, ui.ctx());
            self.config.save();
        }

        egui::Panel::top("top").show(ui, |ui| {
            self.menu_bar(ui);
        });
        if self.show_options {
            self.options_window(ui.ctx());
        }
        self.log_panel(ui);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                self.tab_bar(ui);
                ui.add_space(8.0);
                match self.tab {
                    Tab::Watch => self.watch_tab(ui),
                    Tab::Broadcast => self.broadcast_tab(ui),
                }
            });
        // Drawn last, on its own foreground layer, so it floats over the panels
        // without taking part in their layout.
        self.toast_overlay(ui);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

/// Drop the copy toast once its window has closed.
///
/// `copied` is what arms the repaint guard in [`App::draw`] after a session has
/// gone idle, so it has to expire: before the fix it was set on the first copy
/// and never cleared, and the app asked for a repaint every tick forever.
/// Extracted (with the window length injected) so that regression is testable
/// without a live window.
fn expire_copied(
    copied: Option<(&'static str, Instant)>,
    toast: Duration,
) -> Option<(&'static str, Instant)> {
    copied.filter(|(_, at)| at.elapsed() < toast)
}

/// Truncate a string to `max` chars, adding an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The status-dot breath: a shallow ~2-second pulse, brightness bounded to
/// `0.6..=1.0` of the base colour so "live" reads at a glance without blinking
/// out. Mirrors the viewers cell in the broadcast tab.
fn pulse_factor(secs: f32) -> f32 {
    0.6 + 0.4 * (0.5 + 0.5 * (secs * std::f32::consts::TAU * 0.5).cos())
}

/// The copy-box click flash: a linear fade from `1.0` to `0.0` over `window`,
/// clamped outside it. Drives a brief brighten of the box fill and glyph.
fn flash_alpha(elapsed: Duration, window: Duration) -> f32 {
    if window.is_zero() {
        return 0.0;
    }
    (1.0 - elapsed.as_secs_f32() / window.as_secs_f32()).clamp(0.0, 1.0)
}

/// Install the app's tuned dark theme once, before the first frame.
///
/// egui ships an OS-following stock palette; this pins a deliberate dark look
/// with an accent-tinted selection/hover so the app does not depend on the host
/// OS theme. `all_styles_mut` retunes both the dark and light variants, so the
/// palette stays coherent even if the theme preference is ever switched, while
/// `set_theme` makes dark the deterministic default across platforms. Deltas are
/// kept small: a slightly deeper neutral ground, an accent selection, and gentle
/// widget and spacing bumps.
fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        // A touch deeper than egui's stock dark so panels read as deliberate.
        v.panel_fill = egui::Color32::from_rgb(24, 26, 30);
        v.window_fill = egui::Color32::from_rgb(30, 33, 38);
        v.faint_bg_color = egui::Color32::from_rgb(38, 42, 48);
        // Accent-tinted selection and hover, reused by the copy-box stroke.
        v.selection.bg_fill = ACCENT.gamma_multiply(0.28);
        v.selection.stroke.color = ACCENT;
        // Nudge the interactive surfaces toward the accent without shouting.
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(44, 48, 54);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(44, 48, 54);
        v.widgets.hovered.weak_bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.30);
        v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.30);
        // A little breathing room and softer corners.
        style.spacing.item_spacing = egui::vec2(10.0, 7.0);
        style.spacing.window_margin = egui::Margin::same(14);
        v.window_corner_radius = egui::CornerRadius::same(8);
        v.menu_corner_radius = egui::CornerRadius::same(8);
    });
}

/// Open the GUI.
///
/// Creates the background runtime, loads config, installs the tee logger, wires
/// the stats channel and consumer task, and runs eframe until the window closes.
pub fn run() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: unable to start runtime: {e}");
            std::process::exit(1);
        }
    };
    let handle = runtime.handle().clone();

    let mut config = Config::load();
    let secret = config.ensure_secret();
    let log = log::new_ring();
    log::install(log.clone(), config.verbosity);

    let whip = state::whip_url(&config.broadcast_host, config.broadcast_port);
    let state = Arc::new(Mutex::new(SharedState::new(secret, whip)));

    let (stats_tx, stats_rx) = mpsc::unbounded_channel::<StreamStats>();
    // Keep one sender alive for the whole session so the consumer task does not
    // see the channel close between broadcast start/stops.
    let keepalive = stats_tx.clone();

    let app = App::new(handle.clone(), config, state.clone(), log.clone(), stats_tx);
    let consumer_state = state.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("streampipe"),
        ..Default::default()
    };

    let result = eframe::run_native(
        "streampipe",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            apply_theme(&ctx);
            handle.spawn(state::consume_stats(stats_rx, consumer_state, ctx));
            Ok(Box::new(app))
        }),
    );

    // Close the stats channel so the consumer task exits, then give any teardown
    // (e.g. the player kill task) a moment to run before the runtime stops.
    drop(keepalive);
    runtime.shutdown_timeout(Duration::from_millis(1000));

    // A normal window close returns `Ok`. An `Err` means the window/GL context
    // never came up (e.g. no display); report that as a failed launch rather
    // than exiting 0, so launchers and scripts do not see false success.
    if let Err(e) = result {
        eprintln!("error: gui failed: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    /// An `App` wired to throwaway state, plus the runtime it borrows a handle
    /// from (kept alive by the caller).
    fn test_app() -> (App, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = Config::default();
        let whip = state::whip_url(&config.broadcast_host, config.broadcast_port);
        let shared = Arc::new(Mutex::new(SharedState::new(SecretKey::generate(), whip)));
        let (stats_tx, _stats_rx) = mpsc::unbounded_channel();
        let app = App::new(
            rt.handle().clone(),
            config,
            shared,
            log::new_ring(),
            stats_tx,
        );
        (app, rt)
    }

    /// Drive real frames of the UI headlessly and report the repaint delay the
    /// viewport settled on. `Duration::MAX` means "do not wake me up".
    ///
    /// egui spends its first couple of passes priming (font texture, deferred
    /// repaint bookkeeping) and reports a zero delay there, so the settled value
    /// is read after three passes.
    fn settled_repaint_delay(app: &mut App) -> Duration {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            ..Default::default()
        };
        let mut delay = Duration::ZERO;
        for _ in 0..3 {
            let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
            delay = out
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|v| v.repaint_delay)
                .expect("root viewport");
            // There is no backend here to consume the font texture upload, so clear
            // it rather than letting it panic on drop.
            out.textures_delta.clear();
        }
        delay
    }

    /// Regression: `copied` used to be set on the first copy box click and never
    /// cleared, so the guard in [`App::draw`] stayed armed and an idle window
    /// repaints every tick forever.
    /// Assert the viewport has scheduled *some* follow-up repaint. Compared as
    /// "less than `Duration::MAX`" rather than exactly [`REPAINT`]: egui shaves
    /// the time spent inside the passes off the reported delay, and revealing the
    /// toast Area forces one settling frame.
    fn assert_awake(delay: Duration) {
        assert!(
            delay < Duration::MAX,
            "expected a scheduled repaint, got {delay:?}"
        );
        assert!(delay <= REPAINT, "repaints should not lag {REPAINT:?}");
    }

    #[test]
    fn idle_window_falls_asleep_once_the_copy_toast_expires() {
        let (mut app, _rt) = test_app();

        // Idle, nothing ever copied: no repaint is requested at all.
        assert_eq!(settled_repaint_delay(&mut app), Duration::MAX);

        // A click arms the toast, so frames keep coming (even while idle) to fade it.
        app.copied = Some(("whip", Instant::now()));
        assert_awake(settled_repaint_delay(&mut app));

        // The toast window has closed: the idle app must stop asking for frames and
        // clear the flag that armed the guard. This is the regression — before the
        // fix `copied` stayed set, so this stayed awake for ever.
        app.copied = Some(("whip", Instant::now() - TOAST - Duration::from_millis(1)));
        assert_eq!(
            settled_repaint_delay(&mut app),
            Duration::MAX,
            "an expired toast must not keep an idle window repainting"
        );
        assert_eq!(app.copied, None, "the expired toast flag is cleared");
    }

    /// A live session always repaints, toast or not.
    #[test]
    fn an_active_session_always_repaints() {
        let (mut app, _rt) = test_app();
        app.state.lock().unwrap().mode = Mode::Broadcasting;
        assert_awake(settled_repaint_delay(&mut app));
    }

    /// Render one real frame of the UI headlessly and return what was painted.
    fn render_shapes(app: &mut App, size: egui::Vec2) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        out.shapes
    }

    /// Render the live broadcast tab with viewers at the minimum window size, to
    /// exercise the click-to-copy boxes, the viewers cell and the floating toast for
    /// real: egui does the layout, and we check it paints, that the toast adds its
    /// own painting, and that nothing escapes the window.
    #[test]
    fn broadcast_tab_renders_at_min_size() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        {
            let mut g = app.state.lock().unwrap();
            g.mode = Mode::Broadcasting;
            // A realistically long ticket: the copy box must elide it so it neither
            // runs off the box nor past the window edge.
            g.ticket = Some("b".repeat(140));
            g.stats = Some(StreamStats {
                viewers: 3,
                ..Default::default()
            });
        }
        let min = egui::vec2(560.0, 420.0);
        app.copied = Some(("ticket", Instant::now()));

        let with_toast = render_shapes(&mut app, min);
        assert!(!with_toast.is_empty(), "the broadcast tab painted nothing");

        // Nothing escapes the window: no overflow at the minimum size.
        let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
        for shape in &with_toast {
            assert!(
                window.expand(1.0).contains_rect(shape.clip_rect),
                "painted rect {:?} escapes the window",
                shape.clip_rect
            );
        }

        // Text extents (not just clip rects) stay inside the window: a long ticket
        // is elided, so no glyph run spills past the right edge. `clip_rect` is the
        // enclosing panel clip and would pass even if the galley overflowed, so the
        // galley's own width is checked here — the regression the copy box had.
        for shape in &with_toast {
            if let egui::epaint::Shape::Text(t) = &shape.shape {
                let right = t.pos.x + t.galley.rect.width();
                assert!(
                    right <= min.x + 1.0,
                    "text {:?} (x={:.0}, w={:.0}) runs past the window edge",
                    t.galley.text().chars().take(12).collect::<String>(),
                    t.pos.x,
                    t.galley.rect.width()
                );
            }
        }

        // The toast contributes its own painting on top of the tab.
        app.copied = None;
        let without_toast = render_shapes(&mut app, min);
        assert!(
            with_toast.len() > without_toast.len(),
            "the copy toast should add painting, got {} vs {}",
            with_toast.len(),
            without_toast.len()
        );
    }

    /// Render the Watch tab at the minimum window size and assert nothing escapes
    /// the window. The Watch tab has a multiline ticket box, two combo boxes and a
    /// buffer field, so it is checked separately from the broadcast tab.
    #[test]
    fn watch_tab_renders_at_min_size() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "a-friend-ticket-string-that-is-long-ish".into();
        let min = egui::vec2(560.0, 420.0);
        let shapes = render_shapes(&mut app, min);
        assert!(!shapes.is_empty(), "the watch tab painted nothing");
        let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
        for shape in &shapes {
            assert!(
                window.expand(1.0).contains_rect(shape.clip_rect),
                "painted rect {:?} escapes the window",
                shape.clip_rect
            );
        }
    }

    /// Count the interactive rects the Options window registers with the given
    /// mode, rendering only that window so the tab panels' own gating cannot
    /// confound the comparison.
    fn options_interactive_rects(mode: Mode) -> usize {
        let (mut app, _rt) = test_app();
        app.state.lock().unwrap().mode = mode;
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            ..Default::default()
        };
        // A few passes so the window and its grid settle at their final size.
        for _ in 0..3 {
            let mut out = ctx.run_ui(raw.clone(), |ui| {
                app.options_window(ui.ctx());
            });
            out.textures_delta.clear();
        }
        ctx.interactive_rects_last_pass().len()
    }

    /// Regression (AC3): the Options "Streaming" inputs must leave the interactive
    /// set while a session is live and return to it when idle. `interactive_rects_last_pass`
    /// filters out disabled widgets, so this count distinguishes disabled from enabled
    /// where an in-bounds/no-crash render check would pass either way.
    #[test]
    fn options_streaming_fields_are_disabled_while_a_session_is_active() {
        let idle = options_interactive_rects(Mode::Idle);
        let broadcasting = options_interactive_rects(Mode::Broadcasting);
        assert!(
            broadcasting < idle,
            "streaming fields should be non-interactive while broadcasting \
             ({broadcasting} interactive rects vs {idle} idle)"
        );
    }

    #[test]
    fn expire_copied_leaves_a_live_toast_alone() {
        let at = Instant::now();
        assert!(expire_copied(Some(("ticket", at)), Duration::from_secs(10)).is_some());
        assert!(expire_copied(Some(("ticket", at)), Duration::ZERO).is_none());
        assert!(expire_copied(None, Duration::from_secs(10)).is_none());
    }

    #[test]
    fn truncate_stays_within_budget() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hello world", 5).chars().count(), 5);
    }

    #[test]
    fn pulse_factor_peaks_and_troughs_within_bounds() {
        assert!((pulse_factor(0.0) - 1.0).abs() < 1e-6, "peak at t=0");
        assert!((pulse_factor(1.0) - 0.6).abs() < 1e-6, "trough at t=1");
        let mut secs = 0.0;
        while secs <= 4.0 {
            let f = pulse_factor(secs);
            assert!(
                (0.6..=1.0).contains(&f),
                "pulse_factor({secs}) = {f} outside 0.6..=1.0"
            );
            secs += 0.1;
        }
    }

    #[test]
    fn flash_alpha_fades_linearly() {
        assert_eq!(flash_alpha(Duration::ZERO, FLASH), 1.0);
        assert_eq!(flash_alpha(FLASH, FLASH), 0.0);
        assert_eq!(
            flash_alpha(FLASH * 2, FLASH),
            0.0,
            "clamped past the window"
        );
        let half = flash_alpha(FLASH / 2, FLASH);
        assert!(half > 0.0 && half < 1.0, "mid-window is between 0 and 1");
        assert!(
            flash_alpha(Duration::from_millis(100), FLASH)
                > flash_alpha(Duration::from_millis(200), FLASH),
            "monotonically decreasing"
        );
    }

    /// Count the interactive rects on the Watch tab that are the size of the
    /// full-width "Watch Stream" button (it is `add_sized` to 36 tall; the tab
    /// strip buttons are 32 and the multiline ticket box ~70, so a 34..=38 band
    /// isolates the primary action). `interactive_rects_last_pass` filters out
    /// disabled widgets, so a blank ticket — which disables the button — yields zero.
    ///
    /// The faint "paste a ticket to begin" hint is *not* counted: egui registers
    /// hover-sense rects for plain labels too, so a naive total count would tie
    /// (disabled button out, hint in). Filtering by the button's height isolates
    /// the button and makes the gating observable.
    fn watch_button_rects(ticket: &str) -> usize {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = ticket.to_string();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            ..Default::default()
        };
        for _ in 0..3 {
            let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
            out.textures_delta.clear();
        }
        ctx.interactive_rects_last_pass()
            .iter()
            .filter(|r| (34.0..=38.0).contains(&r.height()))
            .count()
    }

    #[test]
    fn a_blank_ticket_disables_the_watch_button() {
        assert_eq!(
            watch_button_rects("abc"),
            1,
            "a real ticket shows one interactive Watch button"
        );
        assert_eq!(
            watch_button_rects("   "),
            0,
            "a blank ticket disables the Watch button, dropping it from the set"
        );
    }

    /// A focused, window-keyboard-focused raw input at the default window size.
    fn focused_raw(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            focused: true,
            events,
            ..Default::default()
        }
    }

    /// Run one headless frame, discarding the font-texture upload no backend eats.
    fn frame(ctx: &egui::Context, app: &mut App, raw: egui::RawInput) {
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
    }

    /// A single Enter key press event.
    fn enter_key() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Autofocus grabs a prefilled ticket (the "press Enter to replay" case) but
    /// leaves an empty box unfocused, so an idle Watch tab still falls asleep.
    #[test]
    fn autofocus_grabs_a_prefilled_ticket_but_not_an_empty_one() {
        // Prefilled: focus is taken within a couple of frames, no click issued.
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "a-persisted-ticket".into();
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, focused_raw(vec![]));
        frame(&ctx, &mut app, focused_raw(vec![]));
        assert!(
            ctx.memory(|m| m.focused()).is_some(),
            "a prefilled ticket autofocuses so Enter can start it"
        );

        // Empty: nothing focuses (an empty box can never submit), which is exactly
        // what keeps the repaint-sleep regression green under `focused: true`.
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, focused_raw(vec![]));
        frame(&ctx, &mut app, focused_raw(vec![]));
        assert_eq!(
            app.watch_ticket, "",
            "the headless app opens with an empty ticket"
        );
        assert!(
            ctx.memory(|m| m.focused()).is_none(),
            "an empty ticket must not autofocus (no blink repaints while idle)"
        );
    }

    /// Regression for the concrete wart: Enter on the focused ticket box used to
    /// drop a stray newline into the multiline field. Now Enter never inserts a
    /// newline, and an invalid ticket never starts (matching the button path).
    #[test]
    fn enter_on_the_focused_ticket_never_inserts_a_newline() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "not-a-real-ticket".into();
        let ctx = egui::Context::default();
        // Two frames to let autofocus settle, then press Enter with focus held.
        frame(&ctx, &mut app, focused_raw(vec![]));
        frame(&ctx, &mut app, focused_raw(vec![]));
        assert!(ctx.memory(|m| m.focused()).is_some(), "ticket autofocused");
        frame(&ctx, &mut app, focused_raw(vec![enter_key()]));

        assert!(
            !app.watch_ticket.contains('\n'),
            "Enter must not drop a newline into the ticket box"
        );
        assert_eq!(app.watch_ticket, "not-a-real-ticket", "ticket text untouched");
        assert_eq!(
            app.state.lock().unwrap().mode,
            Mode::Idle,
            "an invalid ticket surfaces an inline error but never starts"
        );
    }

    /// A blank ticket never starts a session, focused Enter or not.
    #[test]
    fn enter_on_a_blank_ticket_does_not_start() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        // Force focus on the (blank) box to isolate the blank gate from autofocus.
        app.focus_primary = false;
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, focused_raw(vec![enter_key()]));
        assert_eq!(
            app.state.lock().unwrap().mode,
            Mode::Idle,
            "Enter on a blank ticket does not start a session"
        );
    }

    /// On the Broadcast tab, when idle and focused, the start button owns keyboard
    /// focus so Enter/Space can start it (AC4 precondition). Real-app start then
    /// flows through the button's native keyboard `clicked()`.
    #[test]
    fn broadcast_start_button_autofocuses_when_idle() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, focused_raw(vec![]));
        frame(&ctx, &mut app, focused_raw(vec![]));
        assert!(
            ctx.memory(|m| m.focused()).is_some(),
            "the idle start button autofocuses so Enter/Space can start it"
        );
    }
}
