//! The Options panel: a Streaming section (WHIP flags) and a Watching section
//! (default player, per-player paths, buffer, play address, verbosity).
//!
//! Opened from the menu bar. Edits the config live and writes it to disk on
//! Save (and on quit). The default player set here is auto-selected on the
//! Watch tab at open.

use eframe::egui::{self, Context, Ui};

use super::{
    config::{player_key, PLAYERS},
    watch::parse_buffer_ms,
    App,
};

impl App {
    /// Show the Options window, closing it when its close button is pressed.
    pub fn options_window(&mut self, ctx: &Context) {
        // A local `open` avoids borrowing a field of `self` while the content
        // closure also borrows `self` mutably.
        let mut open = true;
        egui::Window::new("Options")
            .default_width(380.0)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                self.options_content(ui);
            });
        if !open {
            self.show_options = false;
        }
    }

    /// The two sections of the Options window.
    fn options_content(&mut self, ui: &mut Ui) {
        // Host, port, token and verbosity are captured when a broadcast starts, so
        // editing them mid-session is inert; grey them out while one is live. The
        // owned snapshot drops its lock immediately, so the grid's `&mut self.config`
        // borrows stay valid.
        let active = super::state::snapshot(&self.state).mode.active();
        egui::CollapsingHeader::new("Streaming")
            .default_open(true)
            .show(ui, |ui| {
                ui.add_enabled_ui(!active, |ui| {
                    egui::Grid::new("opt_stream")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("WHIP host");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.broadcast_host)
                                    .desired_width(200.0),
                            );
                            ui.end_row();

                            ui.label("WHIP port");
                            ui.add(
                                egui::DragValue::new(&mut self.config.broadcast_port)
                                    .range(1..=65535),
                            );
                            ui.end_row();

                            ui.label("Bearer token");
                            let mut token = self.config.bearer_token.clone().unwrap_or_default();
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut token)
                                    .hint_text("none")
                                    .desired_width(200.0),
                            );
                            if resp.changed() {
                                self.config.bearer_token = if token.trim().is_empty() {
                                    None
                                } else {
                                    Some(token)
                                };
                            }
                            ui.end_row();

                            ui.label("Verbosity");
                            ui.add(egui::DragValue::new(&mut self.config.verbosity).range(0..=3));
                            ui.end_row();
                        });
                });
                ui.weak("Host, port and verbosity apply on the next start.");
            });

        ui.add_space(4.0);

        egui::CollapsingHeader::new("Watching")
            .default_open(true)
            .show(ui, |ui| {
                ui.label("Default player");
                ui.horizontal(|ui| {
                    for player in PLAYERS {
                        if ui
                            .radio(self.config.default_player() == player, player_key(player))
                            .clicked()
                        {
                            self.config.default_player = player_key(player).to_string();
                            self.sync_watch_from_config();
                        }
                    }
                });

                ui.add_space(4.0);
                ui.label("Player paths (optional)");
                egui::Grid::new("opt_paths")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        for player in PLAYERS {
                            if player_key(player) == "none" {
                                continue;
                            }
                            ui.label(player_key(player));
                            let mut path = self
                                .config
                                .player_paths
                                .get(player)
                                .unwrap_or_default()
                                .to_string();
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut path)
                                    .hint_text("on PATH")
                                    .desired_width(220.0),
                            );
                            if resp.changed() {
                                self.config.player_paths.set(
                                    player,
                                    if path.trim().is_empty() {
                                        None
                                    } else {
                                        Some(path)
                                    },
                                );
                            }
                            ui.end_row();
                        }
                    });

                ui.add_space(4.0);
                egui::Grid::new("opt_watch")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Buffer (ms)");
                        let mut buffer = self
                            .config
                            .buffer_ms
                            .map(|ms| ms.to_string())
                            .unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut buffer)
                                .hint_text("auto")
                                .desired_width(80.0),
                        );
                        if resp.changed() {
                            self.config.buffer_ms = parse_buffer_ms(&buffer);
                            self.watch_buffer = buffer;
                        }
                        ui.end_row();

                        ui.label("Play address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.play_addr)
                                .desired_width(160.0),
                        );
                        ui.end_row();
                    });
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                self.sync_watch_from_config();
                self.config.save();
                ui.ctx().request_repaint();
            }
            if ui.button("Close").clicked() {
                self.show_options = false;
            }
        });
    }
}
