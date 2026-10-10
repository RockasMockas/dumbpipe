//! The Watch Stream tab: paste a ticket, pick a player, play.

use std::{path::PathBuf, str::FromStr};

use dumbpipe::EndpointTicket;
use eframe::egui::{self, Context, Ui};

use super::{
    config::{player_key, PLAYERS},
    state::{self, Mode, WatchParams},
    truncate, App, ACCENT,
};

/// Parse a buffer field (milliseconds) into an option; blank or invalid is
/// `None` (lowest latency).
pub fn parse_buffer_ms(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

impl App {
    /// Render the Watch Stream tab.
    pub fn watch_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let busy = snap.mode.active();
        let watching = snap.mode == Mode::Watching;
        let starting = snap.mode == Mode::Starting;

        ui.strong("Stream ticket");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.add_sized(
                    [ui.available_width(), 54.0],
                    egui::TextEdit::multiline(&mut self.watch_ticket)
                        .hint_text("paste a ticket from a friend")
                        .background_color(egui::Color32::from_rgba_unmultiplied(80, 200, 120, 14)),
                )
            });
        });

        // A quick history picker for recently watched tickets.
        let history = self.config.ticket_history.clone();
        if !history.is_empty() && !busy {
            ui.horizontal(|ui| {
                ui.weak("Recent:");
                egui::ComboBox::from_id_salt("ticket_history")
                    .selected_text("choose…")
                    .show_ui(ui, |ui| {
                        for ticket in history {
                            if ui
                                .selectable_value(
                                    &mut self.watch_ticket,
                                    ticket.clone(),
                                    truncate(&ticket, 48),
                                )
                                .clicked()
                            {
                                // selection applied by selectable_value
                            }
                        }
                    });
            });
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.strong("Player");
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("watch_player")
                    .selected_text(player_key(self.watch_player))
                    .show_ui(ui, |ui| {
                        for player in PLAYERS {
                            ui.selectable_value(&mut self.watch_player, player, player_key(player));
                        }
                    });
            });
            ui.add_space(12.0);
            ui.strong("Buffer (ms)");
            ui.add_enabled_ui(!busy, |ui| {
                ui.add_sized(
                    [80.0, 20.0],
                    egui::TextEdit::singleline(&mut self.watch_buffer).hint_text("auto"),
                )
            });
        });

        if let Some(path) = self.config.player_paths.get(self.watch_player) {
            ui.weak(format!("player path: {path}"));
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if watching || starting {
                if ui
                    .add(egui::Button::new("■  Stop").min_size(egui::vec2(140.0, 30.0)))
                    .clicked()
                {
                    state::stop(&self.state, ui.ctx());
                }
                ui.label(match snap.mode {
                    Mode::Starting => "connecting…",
                    _ => "playing in a player window",
                });
            } else {
                let button = egui::Button::new("▶  Watch Stream")
                    .min_size(egui::vec2(160.0, 30.0))
                    .fill(ACCENT.gamma_multiply(0.18))
                    .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.55)));
                if ui.add_enabled(!busy, button).clicked() {
                    self.start_watch(ui.ctx());
                }
            }
        });

        if let Some(err) = &snap.error {
            ui.add_space(6.0);
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
        }

        ui.add_space(6.0);
        ui.weak("The player launches on the local RTP ports; close it or press Stop to end.");
    }

    /// Validate the ticket and start a watch session.
    fn start_watch(&mut self, ctx: &Context) {
        let ticket_text = self.watch_ticket.trim().to_string();
        let ticket = match EndpointTicket::from_str(&ticket_text) {
            Ok(ticket) => ticket,
            Err(e) => {
                // Inline error, no spawn.
                self.state.lock().unwrap().error = Some(format!("invalid ticket: {e}"));
                ctx.request_repaint();
                return;
            }
        };
        let addr = ticket.endpoint_addr().clone();
        let player_path = self
            .config
            .player_paths
            .get(self.watch_player)
            .map(PathBuf::from);
        let play_addr = self.config.play_addr.clone();
        let buffer_ms = parse_buffer_ms(&self.watch_buffer);

        // Persist the last-used buffer and ticket. The default player is owned by
        // Options (it is the value auto-selected on the Watch tab at open), so a
        // quick-switch here only affects this session and must not overwrite it.
        self.config.buffer_ms = buffer_ms;
        self.config.note_ticket(&ticket_text);
        self.config.save();

        let handle = state::spawn_watch(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            WatchParams {
                addr,
                player: self.watch_player,
                player_path,
                play_addr,
                buffer_ms,
            },
        );
        self.state.lock().unwrap().session = Some(handle);
    }

    /// Keep the Watch tab's live fields in sync with the config after Options
    /// edits them.
    pub fn sync_watch_from_config(&mut self) {
        self.watch_player = self.config.default_player();
        self.watch_buffer = self
            .config
            .buffer_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
    }
}
