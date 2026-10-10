//! The Start Broadcasting tab: WHIP host/port/token, a start button, then the
//! click-to-copy WHIP URL and ticket plus live statistics.

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    time::Instant,
};

use eframe::egui::{self, Context, Ui};

use super::{
    config::Config,
    flash_alpha,
    state::{self, BindAddrs, BroadcastParams, Mode},
    App, ACCENT, FLASH,
};

/// Horizontal room kept clear at the right edge of a copy box for the copy glyph,
/// so a long value is elided before it reaches the mark.
const GLYPH_GUTTER: f32 = 26.0;

impl App {
    /// Render the Start Broadcasting tab.
    pub fn broadcast_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let active = snap.mode.active();
        let broadcasting = snap.mode == Mode::Broadcasting;
        let starting = snap.mode == Mode::Starting;
        // Read window focus up front so the idle start button can autofocus itself
        // on the first focused frame (immutable read; the layout borrows follow).
        let window_focused = ui.input(|i| i.focused);

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

                        // Optional fixed bind sockets. Empty (the default) binds an
                        // ephemeral port and the ticket changes each session. A
                        // streamer with a static public IP can pin `ip:port` here so
                        // the ticket stays byte stable across sessions.
                        ui.label("Bind IPv4:");
                        let mut v4 = self
                            .config
                            .broadcast_bind_ipv4
                            .clone()
                            .unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut v4)
                                .hint_text("e.g. 203.0.113.7:45678")
                                .desired_width(180.0),
                        );
                        if resp.changed() {
                            self.config.broadcast_bind_ipv4 = if v4.trim().is_empty() {
                                None
                            } else {
                                Some(v4)
                            };
                        }
                        ui.end_row();

                        ui.label("Bind IPv6:");
                        let mut v6 = self
                            .config
                            .broadcast_bind_ipv6
                            .clone()
                            .unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut v6)
                                .hint_text("e.g. [2001:db8::1]:45678")
                                .desired_width(180.0),
                        );
                        if resp.changed() {
                            self.config.broadcast_bind_ipv6 = if v6.trim().is_empty() {
                                None
                            } else {
                                Some(v6)
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
                // Once OBS is in, swap the "waiting" hint for a live viewer count
                // in the accent colour, so a healthy stream reads at a glance.
                let viewers = snap.stats.map(|s| s.viewers).unwrap_or(0);
                if starting {
                    ui.label("starting…");
                } else if viewers > 0 {
                    ui.colored_label(
                        ACCENT,
                        format!(
                            "{viewers} viewer{} live",
                            if viewers == 1 { "" } else { "s" }
                        ),
                    );
                } else {
                    ui.label("waiting for OBS to connect");
                }
            } else {
                let button = egui::Button::new("●  Start Broadcasting")
                    .min_size(egui::vec2(180.0, 30.0))
                    .fill(ACCENT.gamma_multiply(0.18))
                    .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.55)));
                // Autofocus the start control on the first window-focused idle frame so
                // Enter/Space starts it; the Stop button (shown while active) is never
                // autofocused, so a stray Enter can never tear down a live stream. The
                // button's auto id is stable across frames, so focus requested off
                // `resp` sticks to it without needing an explicit id.
                let resp = ui.add_enabled(!active, button);
                if self.focus_primary && window_focused {
                    if !active {
                        resp.request_focus();
                    }
                    self.focus_primary = false;
                }
                // Mouse click and keyboard (Space/Enter on the focused button) both
                // arrive via `clicked()` — a single trigger, no parallel Enter gate.
                if resp.clicked() {
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

            // Remember the ticket we are actually showing, so the most recent
            // broadcast survives restarts (for reference / history). This is *not*
            // re-advertised as the live ticket on the next broadcast — that is
            // always freshly minted from the live endpoint address so viewers get
            // current transport hints. Guarded to a real change so we never
            // rewrite config.json every frame.
            let changed = snap
                .ticket
                .as_ref()
                .map(|t| Some(t) != self.config.broadcast_ticket.as_ref())
                .unwrap_or(false);
            if changed {
                self.config.broadcast_ticket = snap.ticket.clone();
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
        // A faint accent tint reads the box as an interactive input surface. The
        // whole frame is the click target (not just the glyphs), so the copy
        // affordance is easy to hit and the tinted padding never feels dead.
        // A click briefly lifts the tint toward the accent (see `flash_alpha`).
        // `copied` is `Copy`, so reading it here leaves it intact for the click
        // handler below; the click's repaint shows the flash from the next frame.
        let flash = self
            .copied
            .filter(|(k, _)| *k == key)
            .map(|(_, at)| flash_alpha(at.elapsed(), FLASH))
            .unwrap_or(0.0);
        let fill_alpha = (14.0 + flash * 70.0) as u8;
        let frame = egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgba_unmultiplied(
                80, 200, 120, fill_alpha,
            ))
            .inner_margin(egui::Margin::symmetric(10, 6));
        let inner = frame.show(ui, |ui| {
            // Reserve a gutter on the right for the copy glyph so a long ticket
            // neither runs under the mark nor off the window edge. The full string
            // is still copied on click; only the *display* is elided.
            let width = ui.available_width();
            let text_width = (width - GLYPH_GUTTER).max(0.0);
            let display = fit_monospace(ui, text, text_width);
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let color = ui.style().visuals.text_color();
            let galley = ui
                .painter()
                .layout_no_wrap(display, font_id, egui::Color32::PLACEHOLDER);
            // Lay the value out ourselves at the row's left edge rather than via
            // `add_sized`, which was offsetting the galley toward the centre and
            // spilling long values past the box.
            let row = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 22.0));
            ui.allocate_rect(row, egui::Sense::hover());
            let y = row.center().y - galley.rect.height() * 0.5;
            ui.painter().galley(egui::pos2(row.min.x, y), galley, color);
        });
        // Sense the click over the frame's full rect so the padding is live too.
        // `on_hover_text` consumes the response, so interactions are read off it
        // before it is handed to the tooltip.
        let rect = inner.response.rect;
        // A small "copy" glyph (two overlapping sheets) at the right edge, painted
        // non-interactively so it never steals the box's click.
        let glyph = ACCENT.gamma_multiply(0.5 + 0.5 * flash);
        for sheet in copy_glyph_rects(rect) {
            ui.painter().rect_stroke(
                sheet,
                1.0,
                egui::Stroke::new(1.0, glyph),
                egui::StrokeKind::Inside,
            );
        }
        let resp = ui.interact(rect, ui.id().with(("copy", key)), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_stroke(
                rect,
                frame.corner_radius,
                egui::Stroke::new(1.0, ui.style().visuals.selection.stroke.color),
                egui::StrokeKind::Inside,
            );
        }
        if resp.clicked() {
            ui.ctx().copy_text(text.to_string());
            self.copied = Some((key, Instant::now()));
            ui.ctx().request_repaint();
        }
        resp.on_hover_text("click to copy");
    }

    /// The live statistics panel: viewers, bitrate, fps, elapsed.
    fn stats_panel(&mut self, ui: &mut Ui, snap: &state::UiSnapshot) {
        ui.group(|ui| {
            ui.strong("Statistics");
            let stats = snap.stats.unwrap_or_default();
            egui::Grid::new("stats_grid")
                .num_columns(2)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Viewers");
                    viewers_cell(ui, stats.viewers, self.pulse.elapsed().as_secs_f32());
                    ui.end_row();

                    ui.label("Video");
                    ui.strong(
                        egui::RichText::new(format!(
                            "{:.0} kbit/s · {:.1} fps",
                            stats.video_kbps, stats.video_fps
                        ))
                        .monospace(),
                    );
                    ui.end_row();

                    ui.label("Audio");
                    ui.strong(
                        egui::RichText::new(format!("{:.0} kbit/s", stats.audio_kbps)).monospace(),
                    );
                    ui.end_row();

                    ui.label("Elapsed");
                    ui.strong(egui::RichText::new(format_elapsed(stats.elapsed_secs)).monospace());
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

        // Parse the optional fixed bind addresses. Empty means ephemeral; a bad
        // value is surfaced to the UI rather than silently ignored, so a typo does
        // not quietly fall back to an ephemeral socket and a changing ticket.
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().error = Some(e);
                ctx.request_repaint();
                return;
            }
        };

        self.config.save();

        let handle = state::spawn_broadcast(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            BroadcastParams {
                listen,
                url,
                bearer_token,
                bind,
            },
            self.stats_tx.clone(),
        );
        self.state.lock().unwrap().session = Some(handle);
    }
}

/// Parse the configured fixed bind addresses, `None`/empty meaning ephemeral.
///
/// IPv4 is parsed as `ip:port`; IPv6 as `[ip]:port` (the brackets are optional, so
/// a bare `::1:45678` is rejected in favour of the explicit `[::1]:45678` form,
/// matching `SocketAddrV6`'s expected syntax).
fn parse_bind(config: &Config) -> Result<BindAddrs, String> {
    let ipv4 = match config.broadcast_bind_ipv4.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.parse::<SocketAddrV4>().map_err(|e| {
            format!("invalid Bind IPv4 {s:?}: {e} (expected ip:port)")
        })?),
        _ => None,
    };
    let ipv6 = match config.broadcast_bind_ipv6.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.parse::<SocketAddrV6>().map_err(|e| {
            format!("invalid Bind IPv6 {s:?}: {e} (expected [ip]:port)")
        })?),
        _ => None,
    };
    Ok(BindAddrs { ipv4, ipv6 })
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

/// Elide a monospace string so its rendered width fits `max_width`, appending an
/// ellipsis when shortened. Only the *display* is truncated — callers still copy
/// the full string. Measured with the same monospace face the box renders with, so
/// the estimate is exact; the constant advance of a monospace font lets us compute
/// the cut in one step rather than re-laying out per character.
fn fit_monospace(ui: &Ui, text: &str, max_width: f32) -> String {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let full = ui
        .painter()
        .layout_no_wrap(text.to_string(), font_id, egui::Color32::PLACEHOLDER);
    if text.is_empty() || full.rect.width() <= max_width {
        return text.to_string();
    }
    let count = text.chars().count() as f32;
    let advance = full.rect.width() / count;
    if advance <= 0.0 {
        return "…".to_string();
    }
    // Reserve room for the ellipsis itself, then keep whole characters.
    let keep = ((max_width / advance) - 1.0).max(0.0) as usize;
    let kept: String = text.chars().take(keep).collect();
    format!("{kept}…")
}

/// The two sheets of the copy glyph, anchored at the right edge of a copy box.
///
/// Returns `[back, front]`: two small overlapping rounded rects, the front offset
/// up-right of the back, so the pair reads as the universal "copy" mark without
/// relying on a font glyph the bundled faces may not carry.
fn copy_glyph_rects(rect: egui::Rect) -> [egui::Rect; 2] {
    let size = egui::vec2(8.0, 9.0);
    let cx = rect.right() - 16.0;
    let cy = rect.center().y;
    let offset = 3.0;
    let front = egui::Rect::from_center_size(egui::pos2(cx + offset, cy - offset), size);
    let back = egui::Rect::from_center_size(egui::pos2(cx - offset, cy + offset), size);
    [back, front]
}

/// Format a number of seconds as `42s`, `7m13s` or `1h02m03s`.
fn format_elapsed(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty bind fields mean ephemeral (no fixed socket, ticket changes each run).
    #[test]
    fn parse_bind_empty_is_ephemeral() {
        let bind = parse_bind(&Config::default()).unwrap();
        assert_eq!(bind, BindAddrs::default());
    }

    /// A fixed IPv4/IPv6 bind parses through and would pin the ticket's socket.
    #[test]
    fn parse_bind_accepts_fixed_addresses() {
        let cfg = Config {
            broadcast_bind_ipv4: Some(" 203.0.113.7:45678 ".into()),
            broadcast_bind_ipv6: Some("[2001:db8::1]:45678".into()),
            ..Config::default()
        };
        let bind = parse_bind(&cfg).unwrap();
        assert_eq!(
            bind,
            BindAddrs {
                ipv4: Some("203.0.113.7:45678".parse().unwrap()),
                ipv6: Some("[2001:db8::1]:45678".parse().unwrap()),
            }
        );
    }

    /// A malformed bind is surfaced as an error, not silently dropped to ephemeral.
    #[test]
    fn parse_bind_rejects_malformed() {
        let cfg = Config {
            broadcast_bind_ipv4: Some("not-an-address".into()),
            ..Config::default()
        };
        assert!(parse_bind(&cfg).is_err());
    }

    /// The copy glyph's two sheets sit inside the box, are non-degenerate, and the
    /// front reads up-right of the back. Guards against a misplaced or absent glyph
    /// that a full-render shape count would not catch.
    #[test]
    fn copy_glyph_sheets_are_in_bounds_and_offset() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 34.0));
        let [back, front] = copy_glyph_rects(rect);
        for sheet in [back, front] {
            assert!(
                sheet.width() > 0.0 && sheet.height() > 0.0,
                "non-degenerate"
            );
            assert!(
                rect.contains(sheet.min) && rect.contains(sheet.max),
                "in bounds"
            );
        }
        let front_c = front.center();
        let back_c = back.center();
        assert!(front_c.x > back_c.x, "front is right of back");
        assert!(front_c.y < back_c.y, "front is above back");
    }

    /// `fit_monospace` leaves text that already fits untouched, and elides a long
    /// value to a measured width that fits — the fix for the ticket copy box running
    /// off the box and window edge. The full string is still what gets copied; this
    /// only checks the elided display.
    #[test]
    fn fit_monospace_elides_only_when_too_wide() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            let short = "http://127.0.0.1:8080/whip";
            assert_eq!(
                fit_monospace(ui, short, 400.0),
                short,
                "a WHIP URL that fits is unchanged"
            );

            let long = "a".repeat(200);
            let elided = fit_monospace(ui, &long, 200.0);
            assert!(
                elided.chars().count() < 200 && elided.ends_with('…'),
                "a long ticket is elided with an ellipsis, got {elided:?}"
            );
            let w = ui
                .painter()
                .layout_no_wrap(
                    elided.clone(),
                    egui::TextStyle::Monospace.resolve(ui.style()),
                    egui::Color32::PLACEHOLDER,
                )
                .rect
                .width();
            assert!(w <= 200.0, "elided display width {w} must fit 200");
        });
        out.textures_delta.clear();
    }
}
