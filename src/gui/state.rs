//! Shared GUI state and the bridge to the iroh engine.
//!
//! All long-lived, cross-thread state lives behind a single
//! `Arc<Mutex<SharedState>>`. The rule that keeps this sound with a plain
//! `std::sync::Mutex` is that the lock is **never held across an `.await`**:
//! engine code does not touch the mutex, the stats consumer task locks only
//! inside its loop body, and the UI thread locks only to read a snapshot or to
//! kick off a session before spawning.
//!
//! A single [`Endpoint`] is created once (from the persisted secret) and reused
//! across start/stop cycles, so the endpoint identity is stable. Stopping a
//! session is a synchronous [`tokio::task::JoinHandle::abort`], which drops the
//! WHIP `serve` future (freeing the HTTP port), stops accepting, and drops the
//! viewer's player handle (killing the player). The endpoint itself is kept for
//! reuse and only dropped when the app quits.

use std::{
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use dumbpipe::{
    webrtc::{self, Player, StreamStats, ViewerConfig, WhipConfig},
    EndpointTicket, WEBRTC_ALPN,
};
use eframe::egui;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle, time::timeout};

use crate::{udp, CommonArgs, ONLINE_TIMEOUT};

/// What the GUI is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nothing running.
    Idle,
    /// A session is being brought up (binding, dialing, waiting online).
    Starting,
    /// Broadcasting: serving WHIP and forwarding to viewers.
    Broadcasting,
    /// Watching a remote stream in a player.
    Watching,
}

impl Mode {
    /// Whether a session is running or coming up.
    pub fn active(self) -> bool {
        !matches!(self, Mode::Idle)
    }

    /// A short status label for the header.
    pub fn status(self) -> &'static str {
        match self {
            Mode::Idle => "Idle",
            Mode::Starting => "Starting…",
            Mode::Broadcasting => "Broadcasting",
            Mode::Watching => "Watching",
        }
    }
}

/// The cross-thread state shared by the UI thread, the stats consumer task and
/// the engine session task.
#[derive(Debug)]
pub struct SharedState {
    /// The stable endpoint identity.
    secret: SecretKey,
    /// The shared endpoint, created lazily and reused across sessions.
    endpoint: Option<Endpoint>,
    /// The running engine task, if any.
    pub(crate) session: Option<JoinHandle<()>>,
    /// The current mode.
    pub(crate) mode: Mode,
    /// The live statistics of a broadcast, `None` when not broadcasting.
    pub(crate) stats: Option<StreamStats>,
    /// The current broadcast ticket, shown after the endpoint is online.
    pub(crate) ticket: Option<String>,
    /// The WHIP URL to give OBS, tracking the configured host and port.
    whip_url: String,
    /// The last error surfaced to the UI.
    pub(crate) error: Option<String>,
}

impl SharedState {
    /// Create the state from a loaded config's secret and WHIP settings.
    pub fn new(secret: SecretKey, whip_url: String) -> Self {
        SharedState {
            secret,
            endpoint: None,
            session: None,
            mode: Mode::Idle,
            stats: None,
            ticket: None,
            whip_url,
            error: None,
        }
    }
}

/// A locked snapshot of the fields the UI renders each frame.
///
/// Taken under one brief lock so the UI never holds the mutex while building
/// widgets.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub mode: Mode,
    pub stats: Option<StreamStats>,
    pub ticket: Option<String>,
    pub whip_url: String,
    pub error: Option<String>,
}

/// Snapshot the render-relevant state.
pub fn snapshot(state: &Arc<Mutex<SharedState>>) -> UiSnapshot {
    let g = state.lock().unwrap();
    UiSnapshot {
        mode: g.mode,
        stats: g.stats,
        ticket: g.ticket.clone(),
        whip_url: g.whip_url.clone(),
        error: g.error.clone(),
    }
}

/// The WHIP URL for a host and port, e.g. `http://127.0.0.1:8080/whip`.
pub fn whip_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/whip")
}

/// Bring up the shared endpoint, or return the existing one.
///
/// Never holds the mutex across the `bind`, so it is safe with `std::Mutex`.
///
/// A cached endpoint is reused only while it is still open. `listen_whip`
/// closes the endpoint it is handed when it returns — including on an engine
/// error, or when the WHIP bind fails (a port already in use) — and the abort
/// path of `stop` deliberately leaves it open for reuse. So a stored endpoint
/// that reports `is_closed` is a dead handle: handing it back would let the
/// next broadcast bind nothing, `accept` would return `None` at once, and the
/// session would flash to `Broadcasting` and straight back to `Idle` with no
/// error. Drop it and bind a fresh one instead; the identity is unchanged (same
/// secret), so a persisted ticket still points at this endpoint.
async fn ensure_endpoint(state: &Arc<Mutex<SharedState>>) -> crate::Result<Endpoint> {
    let cached = state.lock().unwrap().endpoint.clone();
    if let Some(endpoint) = cached {
        if !endpoint.is_closed() {
            return Ok(endpoint);
        }
        tracing::debug!("the cached endpoint is closed, binding a fresh one");
    }
    let secret = state.lock().unwrap().secret.clone();
    let common = CommonArgs {
        ipv4_addr: None,
        ipv6_addr: None,
        custom_alpn: None,
        // Verbose enough to log the endpoint lifecycle to the log panel.
        verbose: 1,
        verbose_max: 3,
    };
    let alpn = crate::webrtc_alpn(&common)?;
    let endpoint = crate::create_endpoint(secret, &common, vec![alpn]).await?;
    state.lock().unwrap().endpoint = Some(endpoint.clone());
    Ok(endpoint)
}

/// Mark the session failed and return to idle, surfacing `msg` to the UI.
fn fail(state: &Arc<Mutex<SharedState>>, ctx: &egui::Context, msg: String) {
    tracing::error!("{msg}");
    {
        let mut g = state.lock().unwrap();
        g.mode = Mode::Idle;
        g.stats = None;
        g.error = Some(msg);
    }
    ctx.request_repaint();
}

/// The parameters needed to bring up a broadcast.
pub struct BroadcastParams {
    /// The `host:port` string to resolve and serve WHIP on.
    pub listen: String,
    /// The WHIP URL to display (uses the host/port as typed).
    pub url: String,
    /// The bearer token OBS must present, if any.
    pub bearer_token: Option<String>,
    /// The persisted broadcast ticket to re-display, if any.
    pub persisted_ticket: Option<String>,
    /// Whether the WHIP host and port are unchanged since `persisted_ticket` was
    /// issued. Both sides of this comparison use the trimmed host.
    pub host_port_ok: bool,
}

/// Decide whether to re-display a persisted broadcast ticket, or mint a new one.
///
/// Two gates, in order:
///
/// 1. **Host/port rule.** The ticket stays put unless the WHIP ingest endpoint
///    moves, so a changed host or port means a new ticket.
/// 2. **Identity guard.** The ticket must have been issued for *this* endpoint.
///    `broadcast_host`/`broadcast_port` persist independently of `secret_hex`, so
///    a changed identity (`IROH_SECRET` set, or a regenerated secret) alongside an
///    unchanged host/port would otherwise re-advertise a ticket pointing at a dead
///    [`EndpointId`] that no viewer can ever reach.
///
/// A reused ticket carries the per-session transport addresses of the run that
/// minted it, which are stale by then. It still connects because relays forward
/// encrypted traffic keyed on the stable [`EndpointId`], and the viewer's address
/// lookup re-resolves live addresses once the stale ones are unreachable.
pub fn reuse_ticket(
    persisted: Option<String>,
    host_port_ok: bool,
    current_id: EndpointId,
) -> Option<String> {
    if !host_port_ok {
        return None;
    }
    let ticket = persisted?;
    EndpointTicket::from_str(&ticket)
        .ok()
        .filter(|tk| tk.endpoint_addr().id == current_id)
        .map(|_| ticket)
}

/// Start a broadcast session, returning its task handle to store in the state.
///
/// Sets the mode to `Starting` synchronously, then brings the endpoint online,
/// decides which ticket to display (see [`reuse_ticket`]) and runs
/// [`webrtc::listen_whip`] until it returns or is aborted.
pub fn spawn_broadcast(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    params: BroadcastParams,
    stats_tx: mpsc::UnboundedSender<StreamStats>,
) -> JoinHandle<()> {
    {
        let mut g = state.lock().unwrap();
        g.mode = Mode::Starting;
        g.error = None;
        // Show a zeroed stats panel immediately, before any viewer joins.
        g.stats = Some(StreamStats::default());
        g.ticket = None;
    }
    ctx.request_repaint();
    handle.spawn(async move {
        let endpoint = match ensure_endpoint(&state).await {
            Ok(endpoint) => endpoint,
            Err(e) => return fail(&state, &ctx, format!("could not bind endpoint: {e:#}")),
        };
        if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
            tracing::warn!("failed to connect to the home relay");
        }
        let addr = endpoint.addr();
        let fresh = EndpointTicket::new(addr).to_string();
        // Log the live ticket every session. If a reused ticket ever fails to
        // connect, this is the string that would have worked.
        tracing::debug!("fresh ticket for this session: {fresh}");
        let shown = reuse_ticket(params.persisted_ticket, params.host_port_ok, endpoint.id())
            .unwrap_or(fresh);
        let listen = match udp::resolve(&params.listen) {
            Ok(listen) => listen,
            Err(e) => {
                return fail(&state, &ctx, format!("invalid listen address: {e:#}"));
            }
        };
        {
            let mut g = state.lock().unwrap();
            g.ticket = Some(shown);
            g.whip_url = params.url.clone();
            g.mode = Mode::Broadcasting;
        }
        tracing::info!("serving WHIP on {} and waiting for OBS", params.url);
        ctx.request_repaint();

        let cfg = WhipConfig {
            listen,
            bearer_token: params.bearer_token,
            ice_addr: None,
            stats: true,
            stats_tx: Some(stats_tx),
        };
        let result = webrtc::listen_whip(endpoint, cfg).await;

        let mut g = state.lock().unwrap();
        g.mode = Mode::Idle;
        g.stats = None;
        if let Err(e) = result {
            g.error = Some(format!("{e:#}"));
        }
        ctx.request_repaint();
    })
}

/// The parameters needed to start watching a stream.
pub struct WatchParams {
    /// The parsed remote endpoint address.
    pub addr: EndpointAddr,
    /// The player to launch.
    pub player: Player,
    /// An explicit player binary path, if configured.
    pub player_path: Option<PathBuf>,
    /// The local play-out address.
    pub play_addr: String,
    /// The jitter buffer in milliseconds, `None` for lowest latency.
    pub buffer_ms: Option<u64>,
}

/// Start a watch session, returning its task handle to store in the state.
pub fn spawn_watch(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    params: WatchParams,
) -> JoinHandle<()> {
    {
        let mut g = state.lock().unwrap();
        g.mode = Mode::Watching;
        g.error = None;
    }
    ctx.request_repaint();
    handle.spawn(async move {
        let endpoint = match ensure_endpoint(&state).await {
            Ok(endpoint) => endpoint,
            Err(e) => return fail(&state, &ctx, format!("could not bind endpoint: {e:#}")),
        };
        if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
            tracing::warn!("failed to connect to the home relay");
        }
        let play = match udp::resolve(&params.play_addr) {
            Ok(play) => play,
            Err(e) => return fail(&state, &ctx, format!("invalid play address: {e:#}")),
        };
        let cfg = ViewerConfig {
            addr: params.addr,
            alpn: WEBRTC_ALPN.to_vec(),
            play,
            sdp: None,
            sdp_here: false,
            player: params.player,
            player_path: params.player_path,
            no_launch: false,
            buffer: params.buffer_ms.map(Duration::from_millis),
            stats: true,
        };
        let result = webrtc::connect_whip(endpoint.clone(), cfg).await;

        let mut g = state.lock().unwrap();
        g.mode = Mode::Idle;
        if let Err(e) = result {
            g.error = Some(format!("{e:#}"));
        }
        ctx.request_repaint();
    })
}

/// Abort the running session, if any.
///
/// Synchronous by design: `abort` drops the engine future, which frees the WHIP
/// port, stops accepting, and kills the player. The shared endpoint is kept for
/// reuse.
pub fn stop(state: &Arc<Mutex<SharedState>>, ctx: &egui::Context) {
    let session = state.lock().unwrap().session.take();
    if let Some(session) = session {
        session.abort();
    }
    {
        let mut g = state.lock().unwrap();
        g.mode = Mode::Idle;
        g.stats = None;
    }
    ctx.request_repaint();
}

/// Drain the stats channel into the shared state and repaint.
///
/// This task owns the only async path that touches the mutex, and it locks only
/// inside the loop body (never across an `.await`).
pub async fn consume_stats(
    mut rx: mpsc::UnboundedReceiver<StreamStats>,
    state: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
) {
    while let Some(stats) = rx.recv().await {
        state.lock().unwrap().stats = Some(stats);
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mint a ticket string for a given identity, the way `spawn_broadcast` does
    /// with a live endpoint.
    fn ticket_for(secret: &SecretKey) -> String {
        EndpointTicket::new(EndpointAddr::new(secret.public())).to_string()
    }

    #[test]
    fn reuses_ticket_when_host_port_and_identity_match() {
        let secret = SecretKey::generate();
        let persisted = ticket_for(&secret);
        assert_eq!(
            reuse_ticket(Some(persisted.clone()), true, secret.public()).as_deref(),
            Some(persisted.as_str()),
            "an unchanged host, port and identity must re-display the same ticket"
        );
    }

    #[test]
    fn mints_new_ticket_when_host_or_port_changed() {
        let secret = SecretKey::generate();
        let persisted = ticket_for(&secret);
        assert_eq!(
            reuse_ticket(Some(persisted), false, secret.public()),
            None,
            "the ticket moves when the ingest endpoint moves"
        );
    }

    #[test]
    fn mints_new_ticket_when_identity_changed() {
        // The host/port gate alone is not enough. `broadcast_host`/`broadcast_port`
        // persist independently of `secret_hex`, so a changed identity with an
        // unchanged host/port would otherwise re-advertise a ticket pointing at a
        // dead `EndpointId` that no viewer can ever reach.
        let old = SecretKey::generate();
        let new = SecretKey::generate();
        let persisted = ticket_for(&old);
        assert_eq!(
            reuse_ticket(Some(persisted), true, new.public()),
            None,
            "a ticket from a different identity is never reused"
        );
    }

    #[test]
    fn mints_new_ticket_when_nothing_persisted() {
        let secret = SecretKey::generate();
        assert_eq!(reuse_ticket(None, true, secret.public()), None);
    }

    #[test]
    fn mints_new_ticket_when_persisted_ticket_is_unparsable() {
        // A hand-edited or truncated config must not surface a broken ticket.
        let secret = SecretKey::generate();
        assert_eq!(
            reuse_ticket(
                Some("definitely-not-a-ticket".into()),
                true,
                secret.public()
            ),
            None
        );
    }

    /// A cached endpoint is reused while open, but a closed one (as `listen_whip`
    /// leaves it on any early return, e.g. a WHIP bind failure) must be replaced
    /// with a live handle so the next broadcast actually binds. Regression: the
    /// old `ensure_endpoint` handed back the closed endpoint, whose `accept`
    /// returns `None` at once, so a "port in use → fix port → Start again" cycle
    /// flashed `Broadcasting` and fell back to `Idle` while serving nothing.
    #[tokio::test]
    async fn a_closed_endpoint_is_replaced_on_the_next_session() {
        let state = Arc::new(Mutex::new(SharedState::new(
            SecretKey::generate(),
            whip_url("127.0.0.1", 8080),
        )));

        let first = ensure_endpoint(&state).await.expect("bind");
        // While open, the same endpoint is reused across calls.
        let reused = ensure_endpoint(&state).await.expect("reuse");
        assert!(!reused.is_closed());

        // `listen_whip` closes the endpoint it was handed when it returns.
        first.close().await;
        assert!(first.is_closed());

        // The next session must get a fresh, live endpoint, not the dead one.
        let fresh = ensure_endpoint(&state).await.expect("rebind");
        assert!(
            !fresh.is_closed(),
            "a closed endpoint must be replaced with a live one"
        );
        // Identity is stable across the rebind (same secret), so a persisted
        // ticket still resolves to this endpoint.
        assert_eq!(fresh.id(), reused.id());
    }
}
