//! The Options panel: a Broadcasting section (WHIP flags and the current ticket)
//! and a Watching section
//! (default player, per-player paths, buffer, play address, verbosity).
//!
//! Opened from the menu bar. Edits the config live and writes it to disk on
//! Save (and on quit). The default player set here is auto-selected on the
//! Watch tab at open.

use eframe::egui::{self, Context, Ui};

use super::{config::{player_key, PLAYERS}, App};

impl App {
    /// Show the Options window, closing it when its close button is pressed or
    /// the Escape key is hit.
    pub fn options_window(&mut self, ctx: &Context) {
        // A local `open` avoids borrowing a field of `self` while the content
        // closure also borrows `self` mutably.
        let mut open = true;
        // Span the full width of the viewport (minus a small margin) and pin it
        // there so it snaps open edge-to-edge on the first frame — no fade and no
        // deferred auto-sizing pass that would otherwise stall the width until the
        // mouse moves. Anchored top-left; still vertically resizable.
        let margin = 8.0;
        let width = (ctx.viewport_rect().width() - margin * 2.0).max(320.0);
        egui::Window::new("Settings")
            .anchor(egui::Align2::LEFT_TOP, [margin, margin])
            .default_width(width)
            .min_width(width)
            .max_width(width)
            .fade_in(false)
            .resizable([false, true])
            .open(&mut open)
            .show(ctx, |ui| {
                self.options_content(ui);
            });
        if !open {
            self.show_options = false;
        }
        // Escape closes the overlay too, mirroring the close button.
        if self.show_options && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_options = false;
        }
    }

    /// The two sections of the Options window.
    fn options_content(&mut self, ui: &mut Ui) {
        // The ticket is reset live, so grey it out while a session is active.
        let active = super::state::snapshot(&self.state).mode.active();
        egui::CollapsingHeader::new("Broadcasting")
            .default_open(true)
            .show(ui, |ui| {
                ui.label("Current Ticket:");
                let snap = super::state::snapshot(&self.state);
                let ticket = snap
                    .ticket
                    .clone()
                    .or_else(|| self.config.broadcast_ticket.clone())
                    .unwrap_or_else(|| "no ticket yet — press Reset Ticket".into());
                let requested = self.ticket_row(
                    ui,
                    "opt_ticket",
                    &ticket,
                    !active,
                    "Reset Ticket",
                    "Resetting…",
                );
                if requested {
                    self.reset_ticket(ui.ctx());
                }
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
                                    .hint_text("Default")
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
                        ui.label("Player RTC address");
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
