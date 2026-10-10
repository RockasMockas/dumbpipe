//! The Start Broadcasting tab: WHIP host/port/token, a start button, then the
//! click-to-copy WHIP URL and ticket plus live statistics.

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    time::Instant,
};

use eframe::egui::{self, Context, Ui};
use iroh::SecretKey;

use super::{
    config::Config,
    flash_alpha,
    state::{self, BindAddrs, BroadcastParams, Mode},
    App, ACCENT, FLASH,
};

/// Horizontal room kept clear at the right edge of a copy box for the copy glyph,
/// so a long value is elided before it reaches the mark.
const GLYPH_GUTTER: f32 = 26.0;
/// Height of the value line inside a copy box.
const COPY_ROW_H: f32 = 22.0;
/// Vertical inner margin of a copy box's frame; doubled, it pads the value line.
const COPY_V_MARGIN: f32 = 6.0;
/// Horizontal inner margin of a copy box's frame; the frame's outer width is the
/// value line's `width` plus twice this, so callers must reserve it to avoid the
/// box (and any trailing button) overflowing past the right edge.
const COPY_H_MARGIN: f32 = 10.0;

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

        // Advanced settings: host, port, bearer token and optional fixed bind
        // sockets, prefilled. Collapsed by default so the common case is just the
        // start button. Each field is a full-width box with its label stacked above
        // it, at a larger (1.3×) font, so the panel reads as one unified column.
        egui::CollapsingHeader::new("Advanced settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!active, |ui| {
                    let big = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
                    let prop = egui::FontId::proportional(big);
                    let box_h = big * 1.7;
                    // Full-width editable field with its label on the line above.
                    let field = |ui: &mut Ui,
                                     label: &str,
                                     hint: Option<&str>,
                                     text: &mut String| {
                        ui.label(egui::RichText::new(label).size(big));
                        let mut te =
                            egui::TextEdit::singleline(text).font(prop.clone());
                        if let Some(h) = hint {
                            te = te.hint_text(egui::RichText::new(h).size(big));
                        }
                        ui.add_sized([ui.available_width(), box_h], te);
                    };

                    field(ui, "WHIP host", None, &mut self.config.broadcast_host);

                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("WHIP port").size(big));
                    let mut port = self.config.broadcast_port.to_string();
                    let resp = ui.add_sized(
                        [ui.available_width(), box_h],
                        egui::TextEdit::singleline(&mut port).font(prop.clone()),
                    );
                    if resp.changed() {
                        if let Ok(p) = port.trim().parse::<u16>() {
                            if p >= 1 {
                                self.config.broadcast_port = p;
                            }
                        }
                    }

                    ui.add_space(8.0);
                    let mut token = self.config.bearer_token.clone().unwrap_or_default();
                    field(ui, "Bearer token", Some("none"), &mut token);
                    if token != self.config.bearer_token.clone().unwrap_or_default() {
                        self.config.bearer_token = if token.trim().is_empty() {
                            None
                        } else {
                            Some(token)
                        };
                    }

                    // Optional fixed bind sockets. Empty (the default) binds an
                    // ephemeral port and the ticket changes each session. A
                    // streamer with a static public IP can pin `ip:port` here so
                    // the ticket stays byte stable across sessions.
                    ui.add_space(8.0);
                    let mut v4 = self
                        .config
                        .broadcast_bind_ipv4
                        .clone()
                        .unwrap_or_default();
                    field(
                        ui,
                        "Bind IPv4:",
                        Some("e.g. 203.0.113.7:45678"),
                        &mut v4,
                    );
                    self.config.broadcast_bind_ipv4 = if v4.trim().is_empty() {
                        None
                    } else {
                        Some(v4)
                    };

                    ui.add_space(8.0);
                    let mut v6 = self
                        .config
                        .broadcast_bind_ipv6
                        .clone()
                        .unwrap_or_default();
                    field(
                        ui,
                        "Bind IPv6:",
                        Some("e.g. [2001:db8::1]:45678"),
                        &mut v6,
                    );
                    self.config.broadcast_bind_ipv6 = if v6.trim().is_empty() {
                        None
                    } else {
                        Some(v6)
                    };
                });
            });

        // Once live, reveal the two click-to-copy boxes and the statistics. While
        // idle, still surface the saved ticket so it can be copied and refreshed for
        // the next broadcast — but never offer a refresh live, since rebinding the
        // endpoint mid-stream would tear down the running session. These sit above
        // the primary action, which is pinned to the bottom of the tab.
        if broadcasting {
            ui.add_space(12.0);
            ui.strong("Give OBS this WHIP server URL:");
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, "whip", &snap.whip_url, w);
            ui.add_space(6.0);
            ui.strong("Send friends this ticket to watch:");
            let ticket = snap.ticket.clone().unwrap_or_else(|| "…".into());
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, "ticket", &ticket, w);

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
        } else {
            ui.add_space(12.0);
            ui.strong("Send friends this ticket to watch:");
            let ticket = snap
                .ticket
                .clone()
                .or_else(|| self.config.broadcast_ticket.clone())
                .unwrap_or_else(|| "no ticket yet — press Refresh Relay".into());
            let requested =
                self.ticket_row(ui, "ticket", &ticket, !active, "Refresh Relay", "Refreshing…");
            if requested {
                self.refresh_relay(ui.ctx());
            }
        }

        if let Some(err) = &snap.error {
            ui.add_space(6.0);
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
        }

        // The primary action is pinned to the bottom of the tab, full width, so it
        // reads as the one big commitment after the (optional) settings and the
        // shareable ticket/stats above it.
        ui.add_space(12.0);
        let bw = ui.available_width();
        if broadcasting || starting {
            // Once OBS is in, swap the "waiting" hint for a live viewer count in the
            // accent colour, so a healthy stream reads at a glance.
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
            if ui
                .add_sized([bw, 36.0], egui::Button::new("■  Stop Broadcasting"))
                .clicked()
            {
                state::stop(&self.state, ui.ctx());
                self.config.save();
            }
        } else {
            let button = egui::Button::new("●  Start Broadcasting")
                .fill(ACCENT.gamma_multiply(0.18))
                .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.55)));
            // Autofocus the start control on the first window-focused idle frame so
            // Enter/Space starts it; the Stop button (shown while active) is never
            // autofocused, so a stray Enter can never tear down a live stream. The
            // button's auto id is stable across frames, so focus requested off
            // `resp` sticks to it without needing an explicit id.
            let resp = ui.add_sized([bw, 36.0], button);
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
    }

    /// A ticket copy box with an optional trailing button on its right edge.
    ///
    /// When `allow` is false the row is just the full-width copy box (used while
    /// broadcasting, where a live rebind would tear down the running session). The
    /// button shows `label`, or `busy_label` while a rebind is already in flight, and
    /// is disabled meanwhile. Returns `true` on the frame the button is clicked, so the
    /// caller dispatches the action (relay refresh vs. full ticket reset).
    pub(crate) fn ticket_row(
        &mut self,
        ui: &mut Ui,
        key: &'static str,
        text: &str,
        allow: bool,
        label: &str,
        busy_label: &str,
    ) -> bool {
        if !allow {
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, key, text, w);
            return false;
        }
        let refreshing = self.state.lock().unwrap().refreshing;
        ui.horizontal(|ui| {
            // Reserve room for the button (and the box frame's horizontal margins) so
            // the box neither overflows the row nor pushes the button offscreen.
            let reserve = 120.0;
            let w = (ui.available_width()
                - reserve
                - ui.spacing().item_spacing.x
                - COPY_H_MARGIN * 2.0)
                .max(0.0);
            self.copy_box(ui, key, text, w);
            // Match the box's rendered height so the button sits flush with it, and
            // tint it with the same accent so the pair reads as one control group.
            let h = self.copy_box_height();
            let label = if refreshing { busy_label } else { label };
            let btn = egui::Button::new(label)
                .min_size(egui::vec2(reserve, h))
                .fill(ACCENT.gamma_multiply(0.10))
                .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.5)));
            ui.add_enabled(!refreshing, btn).clicked()
        })
        .inner
    }

    /// The rendered height of a [`App::copy_box`] row (text line plus the frame's
    /// vertical inner margins), used to size a trailing button to sit flush with it.
    fn copy_box_height(&self) -> f32 {
        COPY_ROW_H + COPY_V_MARGIN * 2.0
    }

    /// Rebind the endpoint (same identity) and mint a fresh saved ticket, picking up a
    /// new relay / ephemeral port hints for the next broadcast.
    fn refresh_relay(&mut self, ctx: &Context) {
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().error = Some(e);
                ctx.request_repaint();
                return;
            }
        };
        self.config.save();
        state::spawn_refresh_ticket(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            bind,
            "Relay refreshed ✓",
        );
    }

    /// Mint a brand-new identity: generate a fresh keypair, persist it, then rebind and
    /// mint a ticket from it. Unlike a relay refresh this changes the endpoint id, so
    /// previously shared tickets stop resolving to this stream.
    pub(crate) fn reset_ticket(&mut self, ctx: &Context) {
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().error = Some(e);
                ctx.request_repaint();
                return;
            }
        };
        let secret = SecretKey::generate();
        self.config.secret_hex = Some(data_encoding::HEXLOWER.encode(&secret.to_bytes()));
        self.config.save();
        state::spawn_reset_ticket(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            secret,
            bind,
            "Ticket reset ✓",
        );
    }


    /// A read-only, selectable box that copies its text on click.
    ///
    /// `width` is the box's laid-out width; callers pass `ui.available_width()` for
    /// a full-row box, or a reduced width when a trailing widget (the refresh button)
    /// shares the row.
    ///
    /// The "Copied" confirmation is drawn by [`App::toast_overlay`] as a floating
    /// pill, so it never reflows the surrounding layout. This widget owns the
    /// click and the hover affordance that advertises it.
    fn copy_box(&mut self, ui: &mut Ui, key: &'static str, text: &str, width: f32) {
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
            .inner_margin(egui::Margin::symmetric(
                COPY_H_MARGIN as i8,
                COPY_V_MARGIN as i8,
            ));
        let inner = frame.show(ui, |ui| {
            // Reserve a gutter on the right for the copy glyph so a long ticket
            // neither runs under the mark nor off the box. The full string is still
            // copied on click; only the *display* is elided.
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
            let row = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, COPY_ROW_H));
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

/// Middle-elide a monospace string so its rendered width fits `max_width`: the
/// first and last runs of characters are kept and the middle is replaced with an
/// ellipsis. Only the *display* is truncated — callers still copy the full string.
///
/// The tail is kept on purpose: a ticket's transport hints (relay, ephemeral ports)
/// change at the end, so a head-only truncation would look identical between refreshes
/// even when the ticket had actually changed. Measured with the same monospace face
/// the box renders with, so the estimate is exact; the constant advance of a monospace
/// font lets us compute the cut in one step rather than re-laying out per character.
fn fit_monospace(ui: &Ui, text: &str, max_width: f32) -> String {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let full = ui
        .painter()
        .layout_no_wrap(text.to_string(), font_id, egui::Color32::PLACEHOLDER);
    if text.is_empty() || full.rect.width() <= max_width {
        return text.to_string();
    }
    let count = text.chars().count();
    let advance = full.rect.width() / count as f32;
    if advance <= 0.0 {
        return "…".to_string();
    }
    // Whole characters that fit, minus one slot for the ellipsis. Split the budget so
    // both ends stay visible; the head gets the leftover so the tail (the part that
    // actually changes) is never squeezed to nothing.
    let budget = ((max_width / advance) as usize).saturating_sub(1);
    if budget < 2 {
        return "…".to_string();
    }
    let tail = (budget / 2).max(1);
    let head = budget - tail;
    let head_s: String = text.chars().take(head).collect();
    let tail_s: String = text.chars().skip(count - tail).collect();
    format!("{head_s}…{tail_s}")
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

    /// `fit_monospace` leaves text that already fits untouched, and middle-elides a
    /// long value to a measured width that fits — keeping both the head and the tail
    /// so a ticket whose transport hints change at the end visibly changes between
    /// refreshes. The full string is still what gets copied; this checks the display.
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

            let long = "abcdefghij".repeat(20);
            let elided = fit_monospace(ui, &long, 200.0);
            assert!(
                elided.contains('…') && elided.chars().count() < long.chars().count(),
                "a long ticket is middle-elided, got {elided:?}"
            );
            assert!(
                elided.starts_with('a') && elided.ends_with('j'),
                "both ends are kept so the changing tail stays visible, got {elided:?}"
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
