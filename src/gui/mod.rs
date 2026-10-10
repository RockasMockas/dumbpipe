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
        }
    }

    /// The top menu bar with the status and the Options entry.
    fn menu_bar(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        egui::menu::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Options", |ui| {
                if ui.button("Settings…").clicked() {
                    self.show_options = !self.show_options;
                    ui.close();
                }
            });
            ui.separator();
            let color = match snap.mode {
                Mode::Idle => egui::Color32::GRAY,
                Mode::Starting => egui::Color32::YELLOW,
                Mode::Broadcasting => egui::Color32::from_rgb(80, 200, 120),
                Mode::Watching => egui::Color32::from_rgb(90, 160, 255),
            };
            ui.colored_label(color, format!("● {}", snap.mode.status()));
            if let Some(err) = &snap.error {
                ui.colored_label(egui::Color32::from_rgb(220, 90, 90), truncate(err, 60));
            }
        });
    }

    /// The tab strip.
    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("streampipe");
            ui.add_space(12.0);
            if ui
                .selectable_label(self.tab == Tab::Watch, "Watch Stream")
                .clicked()
            {
                self.tab = Tab::Watch;
            }
            if ui
                .selectable_label(self.tab == Tab::Broadcast, "Start Broadcasting")
                .clicked()
            {
                self.tab = Tab::Broadcast;
            }
        });
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

    /// Whether a "Copied" toast should show for `key`.
    fn toast_active(&self, key: &str) -> bool {
        self.copied
            .map(|(k, at)| k == key && at.elapsed() < TOAST)
            .unwrap_or(false)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let mode = self.state.lock().unwrap().mode;
        if mode.active() || self.copied.is_some() {
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
        egui::CentralPanel::default().show(ui, |ui| {
            self.tab_bar(ui);
            ui.add_space(8.0);
            match self.tab {
                Tab::Watch => self.watch_tab(ui),
                Tab::Broadcast => self.broadcast_tab(ui),
            }
        });
    }
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
