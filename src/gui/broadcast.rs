//! The Start Broadcasting tab: WHIP host/port/token, a start button, then the
//! click-to-copy WHIP URL and ticket plus live statistics.

use std::time::Instant;

use eframe::egui::{self, Context, Ui};

use super::{
    state::{self, BroadcastParams, Mode},
    App, ACCENT,
};

impl App {
    /// Render the Start Broadcasting tab.
    pub fn broadcast_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let active = snap.mode.active();
        let broadcasting = snap.mode == Mode::Broadcasting;
        let starting = snap.mode == Mode::Starting;

        // Advanced settings: host, port and bearer token, prefilled. Collapsed by
        // default so the common case is just the start button.
        egui::CollapsingHeader::new("Advanced settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!active, |ui| {
                    egui::Grid::new("broadcast_advanced").show(ui, |ui| {
                        ui.label("WHIP host");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.broadcast_host)
                                .desired_width(180.0),
                        );
                        ui.end_row();

                        ui.label("WHIP port");
                        ui.add(
                            egui::DragValue::new(&mut self.config.broadcast_port).range(1..=65535),
                        );
                        ui.end_row();

                        ui.label("Bearer token");
                        let mut token = self.config.bearer_token.clone().unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut token)
                                .hint_text("none")
                                .desired_width(180.0),
                        );
                        if resp.changed() {
                            self.config.bearer_token = if token.trim().is_empty() {
                                None
                            } else {
                                Some(token)
                            };
                        }
                        ui.end_row();
                    });
                });
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if broadcasting || starting {
                if ui
                    .add(
                        egui::Button::new("■  Stop Broadcasting").min_size(egui::vec2(180.0, 30.0)),
                    )
                    .clicked()
                {
                    state::stop(&self.state, ui.ctx());
                    self.config.save();
                }
                ui.label(if starting {
                    "starting…"
                } else {
                    "waiting for OBS to connect"
                });
            } else {
                let button =
                    egui::Button::new("●  Start Broadcasting").min_size(egui::vec2(180.0, 30.0));
                if ui.add_enabled(!active, button).clicked() {
                    self.start_broadcast(ui.ctx());
                }
            }
        });

        if let Some(err) = &snap.error {
            ui.add_space(6.0);
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
        }

        // Once live, reveal the two click-to-copy boxes and the statistics.
        if broadcasting {
            ui.add_space(12.0);
            ui.strong("Give OBS this WHIP server URL:");
            self.copy_box(ui, "whip", &snap.whip_url);
            ui.add_space(6.0);
            ui.strong("Send friends this ticket to watch:");
            let ticket = snap.ticket.clone().unwrap_or_else(|| "…".into());
            self.copy_box(ui, "ticket", &ticket);

            // Persist the ticket we are actually showing, together with the host
            // and port it was issued for, so it survives restarts. Guarded to
            // `broadcasting` (a stopped session must not re-advertise a stale
            // ticket) and to a real change (so we never rewrite config.json every
            // frame).
            let changed = snap
                .ticket
                .as_ref()
                .map(|t| Some(t) != self.config.broadcast_ticket.as_ref())
                .unwrap_or(false);
            if changed {
                self.config.broadcast_ticket = snap.ticket.clone();
                self.config.broadcast_ticket_host = self.config.broadcast_host.trim().to_string();
                self.config.broadcast_ticket_port = self.config.broadcast_port;
                self.config.save();
            }

            ui.add_space(12.0);
            self.stats_panel(ui, &snap);
        }
    }

    /// A read-only, selectable box that copies its text on click.
    ///
    /// The "Copied" confirmation is drawn by [`App::toast_overlay`] as a floating
    /// pill, so it never reflows the surrounding layout. This widget owns the
    /// click and the hover affordance that advertises it.
    fn copy_box(&mut self, ui: &mut Ui, key: &'static str, text: &str) {
        let frame = egui::Frame::group(ui.style());
        let inner = frame.show(ui, |ui| {
            let width = ui.available_width();
            let label =
                egui::Label::new(egui::RichText::new(text).monospace()).sense(egui::Sense::click());
            ui.add_sized([width, 22.0], label)
        });
        // The click lives on the label; the frame's own response only senses hover,
        // so hover is read from the label and painted onto the frame rect using the
        // frame's own corner radius. `on_hover_text` consumes the response, so the
        // interactions are read off it first.
        let label = inner.inner;
        let clicked = label.clicked();
        let hovered = label.hovered();
        label.on_hover_text("click to copy");
        if clicked {
            ui.ctx().copy_text(text.to_string());
            self.copied = Some((key, Instant::now()));
            ui.ctx().request_repaint();
        }
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_stroke(
                inner.response.rect,
                frame.corner_radius,
                egui::Stroke::new(1.0, ui.style().visuals.selection.stroke.color),
                egui::StrokeKind::Inside,
            );
        }
    }

    /// The live statistics panel: viewers, bitrate, fps, elapsed.
    fn stats_panel(&mut self, ui: &mut Ui, snap: &state::UiSnapshot) {
        ui.group(|ui| {
            ui.strong("Statistics");
            let stats = snap.stats.unwrap_or_default();
            egui::Grid::new("stats_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Viewers");
                    viewers_cell(ui, stats.viewers, self.pulse.elapsed().as_secs_f32());
                    ui.end_row();

                    ui.label("Video");
                    ui.strong(format!(
                        "{:.0} kbit/s · {:.1} fps",
                        stats.video_kbps, stats.video_fps
                    ));
                    ui.end_row();

                    ui.label("Audio");
                    ui.strong(format!("{:.0} kbit/s", stats.audio_kbps));
                    ui.end_row();

                    ui.label("Elapsed");
                    ui.strong(format_elapsed(stats.elapsed_secs));
                    ui.end_row();
                });
        });
    }

    /// Resolve the configured host/port and start a broadcast session.
    fn start_broadcast(&mut self, ctx: &Context) {
        let host = self.config.broadcast_host.trim().to_string();
        let port = self.config.broadcast_port;
        if host.is_empty() {
            self.state.lock().unwrap().error = Some("enter a WHIP host".into());
            ctx.request_repaint();
            return;
        }
        let url = state::whip_url(&host, port);
        let listen = format!("{host}:{port}");
        let bearer_token = self.config.bearer_token();
        // The ticket is only supposed to move when the ingest endpoint moves, so
        // gate reuse on the same trimmed host the session will serve on (the raw
        // host may carry whitespace the user typed).
        let host_port_ok =
            host == self.config.broadcast_ticket_host && port == self.config.broadcast_ticket_port;
        self.config.save();

        let handle = state::spawn_broadcast(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            BroadcastParams {
                listen,
                url,
                bearer_token,
                persisted_ticket: self.config.broadcast_ticket.clone(),
                host_port_ok,
            },
            self.stats_tx.clone(),
        );
        self.state.lock().unwrap().session = Some(handle);
    }
}

/// The Viewers cell: a large count with a status dot.
///
/// With viewers it reads bright green and the dot breathes slowly so "live" is
/// obvious at a glance; at zero both sit dim, so an idle broadcast does not shout.
/// `secs` drives the breath and is expected to advance between repaints (repaints
/// already fire while broadcasting).
fn viewers_cell(ui: &mut Ui, viewers: usize, secs: f32) {
    let green = ACCENT;
    let (dot, count) = if viewers > 0 {
        // A shallow 2-second breath: 1.0 down to 0.6, never invisible.
        let breath = 0.6 + 0.4 * (0.5 + 0.5 * (secs * std::f32::consts::TAU * 0.5).cos());
        (green.gamma_multiply(breath), green)
    } else {
        (
            egui::Color32::from_rgb(110, 110, 110),
            egui::Color32::from_rgb(150, 150, 150),
        )
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(egui::RichText::new("●").size(11.0).color(dot));
        ui.label(
            egui::RichText::new(viewers.to_string())
                .strong()
                .size(20.0)
                .color(count),
        );
    });
}

/// Format a number of seconds as `42s`, `7m13s` or `1h02m03s`.
fn format_elapsed(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}
