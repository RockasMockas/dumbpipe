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

/// The status label shown beside Stop while a watch session is up.
///
/// `Starting` reads "connecting…" so the tab never claims a player window exists
/// before the endpoint is online and the play address has resolved.
fn watch_action_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Starting => "connecting…",
        _ => "playing in a player window",
    }
}

/// Whether the "Watch Stream" button is actionable: idle, with a non-blank ticket.
fn watch_can_start(busy: bool, ticket: &str) -> bool {
    !busy && !ticket.trim().is_empty()
}

/// Whether Enter should start a watch session: the ticket box must own keyboard
/// focus, Enter must be pressed, and the same idle-with-non-blank-ticket gate the
/// button uses must hold. Extracted so the keyboard gate is testable without any
/// focus plumbing.
fn watch_enter_starts(focused: bool, enter: bool, busy: bool, ticket: &str) -> bool {
    focused && enter && watch_can_start(busy, ticket)
}

impl App {
    /// Render the Watch Stream tab.
    pub fn watch_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let busy = snap.mode.active();
        let watching = snap.mode == Mode::Watching;
        let starting = snap.mode == Mode::Starting;

        ui.strong("Stream ticket");

        // Read input BEFORE rendering the ticket box, so a key consumed by the
        // widget later in this frame cannot defeat Enter-to-start.
        let ticket_id = egui::Id::new("watch_ticket_input");
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let window_focused = ui.input(|i| i.focused);

        // A framed surface via the TextEdit's OWN frame (not an outer group, which
        // would double the border and eat width). `return_key(None)` suppresses the
        // stray newline Enter would otherwise insert into a multiline box.
        let textedit = egui::TextEdit::multiline(&mut self.watch_ticket)
            .id(ticket_id)
            .hint_text("paste a ticket from a friend")
            .return_key(None::<egui::KeyboardShortcut>)
            .frame(
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgba_unmultiplied(80, 200, 120, 14))
                    .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.4)))
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            );
        let resp = ui
            .add_enabled_ui(!busy, |ui| {
                ui.add_sized([ui.available_width(), 54.0], textedit)
            })
            .inner;

        // Autofocus the ticket only when it already holds a prefilled ticket, on a
        // window that has keyboard focus, and once per switch. Autofocusing an empty
        // box would only blink a caret that can never submit (Enter on a blank ticket
        // is a no-op), and a focused multiline TextEdit schedules ~2Hz caret-blink
        // repaints that would keep an idle Watch tab from ever falling asleep. A
        // prefilled ticket is exactly the "press Enter to replay" case worth grabbing.
        // (`RawInput::default().focused` is `true`, so a bare focus flag is not a
        // sufficient guard against the repaint-sleep regression.)
        if self.focus_primary && window_focused {
            if !busy && !self.watch_ticket.trim().is_empty() {
                resp.request_focus();
            }
            self.focus_primary = false;
        }

        // Enter-to-start, gated on the box actually owning focus and the button's
        // own idle/non-blank conditions. Focus is read off the same `resp`.
        let focused = ui.ctx().memory(|m| m.has_focus(resp.id));
        if watch_enter_starts(focused, enter, busy, &self.watch_ticket) {
            self.start_watch(ui.ctx());
        }

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
                ui.add(
                    egui::TextEdit::singleline(&mut self.watch_buffer)
                        .hint_text("auto")
                        .desired_width(80.0),
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
                ui.label(watch_action_label(snap.mode));
            } else {
                let button = egui::Button::new("▶  Watch Stream")
                    .min_size(egui::vec2(160.0, 30.0))
                    .fill(ACCENT.gamma_multiply(0.18))
                    .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.55)));
                let can_start = watch_can_start(busy, &self.watch_ticket);
                if ui.add_enabled(can_start, button).clicked() {
                    self.start_watch(ui.ctx());
                }
                if !busy && self.watch_ticket.trim().is_empty() {
                    ui.weak("paste a ticket to begin");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_action_label_distinguishes_connecting_from_playing() {
        assert_eq!(watch_action_label(Mode::Starting), "connecting…");
        assert_eq!(
            watch_action_label(Mode::Watching),
            "playing in a player window"
        );
    }

    #[test]
    fn watch_can_start_needs_idle_and_a_non_blank_ticket() {
        assert!(!watch_can_start(false, " "));
        assert!(!watch_can_start(false, ""));
        assert!(watch_can_start(false, "abc"));
        assert!(!watch_can_start(true, "abc"));
    }

    #[test]
    fn watch_enter_starts_needs_focus_enter_and_a_startable_ticket() {
        // Enter starts only when the box owns focus, Enter is pressed, and the
        // button's own idle/non-blank gate holds.
        assert!(watch_enter_starts(true, true, false, "abc"));
        // Not focused → never starts (Enter elsewhere must not fire a watch).
        assert!(!watch_enter_starts(false, true, false, "abc"));
        // No Enter → nothing to trigger.
        assert!(!watch_enter_starts(true, false, false, "abc"));
        // Blank ticket → mirrors the disabled button.
        assert!(!watch_enter_starts(true, true, false, "  "));
        assert!(!watch_enter_starts(true, true, false, ""));
        // Busy (incl. Starting) → mirrors the disabled button.
        assert!(!watch_enter_starts(true, true, true, "abc"));
    }
}
