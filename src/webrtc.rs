//! WebRTC ingest and playback over an iroh connection.
//!
//! Neither mpv nor VLC is a WebRTC client: they have no ICE, no DTLS and no
//! way to negotiate a stream. Dumbpipe therefore terminates WebRTC on both
//! ends and hands the viewer plain RTP plus a generated SDP file:
//!
//! ```text
//! OBS --WHIP/HTTP--> [ host: str0m ] --iroh datagrams--> [ viewer ] --udp--> mpv
//! ```
//!
//! The host is the *media authority*. It knows what the publisher offered and
//! which payload types it actually sends, so it can tell a viewer everything it
//! needs to write a playable SDP ([`crate::sdp::SessionHeader`]), and it gates
//! the media it forwards per viewer so that a viewer that joins in the middle of
//! a GOP starts at a keyframe. Without that, a player joined mid-GOP with no
//! parameter sets shows a grey or corrupt picture until the next IDR, and mpv
//! logs `non-existing PPS` for every packet.
//!
//! The media path is unbuffered: every RTP packet is forwarded as one QUIC
//! datagram as soon as it arrives, so there is no head-of-line blocking. A
//! packet larger than the connection's `max_datagram_size()` is fragmented into
//! several datagrams and reassembled by the viewer, never dropped: on a local
//! network nothing fragments, but across a continent the QUIC datagram limit
//! sits near 1100 bytes and full-size RTP packets would otherwise be lost on
//! every frame.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use clap::ValueEnum;
use iroh::{
    endpoint::{Accepting, Connection, RecvStream, SendDatagramError, SendStream},
    Endpoint, EndpointAddr,
};
use n0_error::{anyerr, bail_any, ensure_any, Result, StdResultExt};
use str0m::{
    change::SdpOffer,
    media::{KeyframeRequestKind, MediaKind as Str0mMediaKind, Mid},
    net::{Protocol, Receive},
    rtp::RtpPacket,
    Candidate, Event, Input, Output, Rtc,
};
use tokio::{
    net::UdpSocket,
    process::Command,
    sync::mpsc,
    time::{interval, sleep, sleep_until, MissedTickBehavior},
};

use crate::{
    rtp::{
        self, reorder_timeout_for, Codec, ParameterSets, Reorder, RtpInfo, TAG_AUDIO, TAG_EPOCH,
        TAG_FRAGMENT, TAG_KEYFRAME_REQ, TAG_SESSION, TAG_VIDEO,
    },
    sdp::{self, MediaHeader, MediaKind, OfferMedia, SessionHeader},
    whip::{self, Answer, WhipRequest},
};

/// The local address media is played out on when the user does not pick one.
pub const DEFAULT_PLAY_ADDR: &str = "127.0.0.1:5004";
/// The address OBS posts its WHIP offer to by default.
pub const DEFAULT_WHIP_ADDR: &str = "127.0.0.1:8080";

/// How often the session header is repeated to every viewer.
///
/// The header is the only way a viewer learns the payload type, codec and
/// parameter sets, so it is sent again and again rather than once: a lost
/// datagram must not cost a viewer the stream.
const HEADER_INTERVAL: Duration = Duration::from_secs(1);
/// How long a viewer waits for its player to open the RTP socket.
const PLAYER_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a viewer waits for media before asking for a keyframe.
const STALL_INTERVAL: Duration = Duration::from_secs(2);
/// The shortest interval between two keyframe requests from one viewer.
///
/// A gap in the stream nudges the encoder to refresh, but `last_keyframe` is
/// refreshed by every keyframe that arrives, so a gap right after an IDR asks
/// for nothing: only a sustained gap with no keyframe for this whole interval
/// sends a PLI. Without that, on a lossy path nearly every frame has a gap and
/// a PLI per gap makes the encoder produce IDR after IDR, which blows up the
/// bitrate and drops even more packets.
const KF_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
/// A path with at least this round-trip time counts as long-distance and gets
/// an automatic jitter buffer for the player.
const WAN_RTT: Duration = Duration::from_millis(50);
/// How often a viewer reports that it is waiting for media.
const WAIT_INTERVAL: Duration = Duration::from_secs(5);
/// How often periodic counters are logged while a tunnel runs.
const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How long a viewer waits before redialing a host that went away.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Capacity of the per-viewer media queue.
const VIEWER_QUEUE: usize = 8192;
/// Size of the buffer used to read from the ICE UDP socket.
const ICE_BUF: usize = 2048;

/// The players `connect-whip` knows how to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Player {
    /// mpv, with its `low-latency` profile.
    #[default]
    #[value(alias = "mvp")]
    Mpv,
    /// VLC, with a small network cache.
    Vlc,
    /// ffplay, from the ffmpeg distribution.
    Ffplay,
    /// Do not launch anything, just write the SDP file.
    None,
}

impl Player {
    /// The command line used to play the SDP file at `sdp`.
    ///
    /// With `buffer` unset the lowest latency flags are used, which is right on
    /// a local network. Across a continent the path jitters and reorders packets,
    /// and a receiver with no buffer drops them, corrupts pictures and loses A/V
    /// sync; `buffer` then gives the player time to absorb that.
    ///
    /// `path` overrides the player binary, for a player that is not on `PATH`.
    pub fn command(
        &self,
        sdp: &Path,
        buffer: Option<Duration>,
        path: Option<&std::path::Path>,
    ) -> Vec<String> {
        let file = sdp.display().to_string();
        // The custom path replaces the binary name, the flags stay the player's.
        let bin = |default: &str| {
            path.map(|p| p.display().to_string())
                .unwrap_or_else(|| default.to_string())
        };
        match self {
            Player::Mpv => {
                let mut cmd = vec![
                    bin("mpv"),
                    "--force-window=immediate".to_string(),
                    // A live stream must start at the live edge, never resume a
                    // watched position from a previous run.
                    "--no-resume-playback".to_string(),
                ];
                // Merge lavf options into one argument so buffer_size,
                // max_delay and reorder_queue_size are not overridden by a
                // second --demuxer-lavf-o. ffmpeg's default reorder_queue_size
                // is 500 packets, which overflows ("jitter buffer full") before
                // max_delay is reached at high bitrate; raise the ceiling so the
                // author's two-stage reorder (dumbpipe + ffmpeg) works as intended.
                let mut lavf_o = "buffer_size=4194304".to_string();
                match buffer {
                    Some(ms) => {
                        cmd.push("--cache=yes".into());
                        cmd.push(format!("--demuxer-readahead-secs={}", ms.as_secs_f64()));
                        // `max_delay` is the RTP demuxer's reorder window in us.
                        lavf_o.push_str(&format!(
                            ",max_delay={},reorder_queue_size=32768",
                            ms.as_micros()
                        ));
                    }
                    None => {
                        cmd.push("--no-cache".into());
                        cmd.push("--profile=low-latency".into());
                        // dumbpipe already strictly reorders RTP, so neuter
                        // ffmpeg's redundant queue to avoid its 100 ms default wait.
                        lavf_o.push_str(",max_delay=0,reorder_queue_size=0");
                    }
                }
                cmd.push(format!("--demuxer-lavf-o={}", lavf_o));
                cmd.push(file);
                cmd
            }
            Player::Vlc => {
                let caching = buffer.map(|d| d.as_millis()).unwrap_or(100);
                vec![
                    bin("vlc"),
                    format!("--network-caching={caching}"),
                    "--no-loop".into(),
                    file,
                ]
            }
            Player::Ffplay => {
                let mut cmd = vec![bin("ffplay")];
                match buffer {
                    Some(ms) => {
                        // Drop corrupt packets instead of feeding them to the
                        // decoder, and let the RTP receiver wait for reordering.
                        // Raise reorder_queue_size above ffmpeg's 500-packet default
                        // so max_delay is honoured without a premature "jitter
                        // buffer full" flush at high bitrate.
                        cmd.extend(["-fflags".into(), "+discardcorrupt".into()]);
                        cmd.extend([
                            "-max_delay".into(),
                            ms.as_micros().to_string(),
                            "-reorder_queue_size".into(),
                            "32768".into(),
                        ]);
                    }
                    None => {
                        // dumbpipe already strictly reorders RTP, so disable
                        // ffmpeg's queue to avoid its 100 ms default wait.
                        cmd.extend(["-fflags".into(), "nobuffer".into()]);
                        cmd.extend([
                            "-max_delay".into(),
                            "0".into(),
                            "-reorder_queue_size".into(),
                            "0".into(),
                        ]);
                    }
                }
                cmd.extend([
                    // Enlarge the UDP/RTP receive socket buffer so a brief decode
                    // hiccup at high bitrate does not overflow the kernel default
                    // (~200 KB) and silently drop inbound RTP.
                    "-buffer_size".into(),
                    "4194304".into(),
                    "-probesize".into(),
                    "1000000".into(),
                    "-analyzeduration".into(),
                    "0".into(),
                    // The SDP refers to rtp/udp, which lavfi's protocol whitelist
                    // rejects by default.
                    "-protocol_whitelist".into(),
                    "file,rtp,udp,crypto,data".into(),
                ]);
                cmd.push(file);
                cmd
            }
            Player::None => vec![file],
        }
    }

    /// Whether a player process is launched at all.
    pub fn launches(&self) -> bool {
        *self != Player::None
    }
}

/// Counters for a running media tunnel, used for logging.
#[derive(Debug, Default)]
struct Counters {
    /// RTP packets forwarded to viewers.
    forwarded: AtomicU64,
    /// Bytes of those packets.
    forwarded_bytes: AtomicU64,
    /// Packets dropped because they were too large for a QUIC datagram.
    dropped: AtomicU64,
    /// Packets dropped because they were not a negotiated media payload type.
    unwanted: AtomicU64,
    /// Packets that could not be delivered.
    undeliverable: AtomicU64,
    /// Fragment datagrams sent for packets too large for one datagram.
    fragmented: AtomicU64,
    /// Keyframe requests.
    keyframes: AtomicU64,
}

impl Counters {
    fn get(&self, counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    fn log_periodic(&self) {
        tracing::debug!(
            "webrtc tunnel stats: {} packets / {} bytes forwarded, \
             {} fragmented, {} oversized dropped, {} unwanted payload types dropped, \
             {} undeliverable, {} keyframe requests",
            self.get(&self.forwarded),
            self.get(&self.forwarded_bytes),
            self.get(&self.fragmented),
            self.get(&self.dropped),
            self.get(&self.unwanted),
            self.get(&self.undeliverable),
            self.get(&self.keyframes),
        );
    }

    fn log_final(&self) {
        tracing::info!(
            "webrtc tunnel closed: {} packets / {} bytes forwarded, \
             {} fragmented, {} oversized dropped, {} unwanted payload types dropped, \
             {} undeliverable, {} keyframe requests",
            self.get(&self.forwarded),
            self.get(&self.forwarded_bytes),
            self.get(&self.fragmented),
            self.get(&self.dropped),
            self.get(&self.unwanted),
            self.get(&self.undeliverable),
            self.get(&self.keyframes),
        );
    }

    /// Record a packet that could not even be fragmented.
    fn record_dropped(&self, size: usize, max: usize) {
        let count = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // log the first one and then every 1000th, so a misconfigured sender
        // is visible without flooding the log.
        if count == 1 || count.is_multiple_of(1000) {
            tracing::warn!(
                "dropped unfragmentable rtp datagram ({size} > {max} bytes), \
                 {count} dropped so far; lower the packet size of the publisher",
            );
        }
    }
}

/// Rolling media statistics of one media kind.
///
/// Counters are for the current [`STATS_INTERVAL`] and are reset when the
/// interval is reported, so the numbers in the line are the numbers of the
/// last five seconds rather than of the whole session.
#[derive(Debug)]
struct MediaTally {
    /// When the current interval started.
    since: Instant,
    /// RTP packets seen in this interval.
    packets: u64,
    /// Bytes of those packets, RTP header included.
    bytes: u64,
    /// Frames completed in this interval, counted on the RTP marker bit.
    frames: u64,
    /// Keyframes seen in this interval.
    keyframes: u64,
    /// Keyframes seen since the session started.
    keyframes_total: u64,
    /// When the last keyframe arrived, to report the keyframe interval.
    last_keyframe: Option<Instant>,
}

impl MediaTally {
    fn new(now: Instant) -> Self {
        MediaTally {
            since: now,
            packets: 0,
            bytes: 0,
            frames: 0,
            keyframes: 0,
            keyframes_total: 0,
            last_keyframe: None,
        }
    }

    /// Whether any media arrived in this interval.
    fn active(&self) -> bool {
        self.packets > 0
    }

    /// The average bitrate of this interval in kbit/s.
    fn kbit_per_second(&self, now: Instant) -> f64 {
        let secs = now.duration_since(self.since).as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.bytes as f64 * 8.0) / secs / 1000.0
    }

    /// Start a new interval.
    fn restart(&mut self, now: Instant) {
        self.since = now;
        self.packets = 0;
        self.bytes = 0;
        self.frames = 0;
        self.keyframes = 0;
    }
}

/// What the publisher is sending, so the operator can see it working.
///
/// Reported every [`STATS_INTERVAL`] by [`Host::report_media`] and again when
/// the input closes.
#[derive(Debug)]
struct StreamReport {
    /// When the first media packet of this session arrived.
    started: Option<Instant>,
    /// The video tally.
    video: MediaTally,
    /// The audio tally.
    audio: MediaTally,
}

impl StreamReport {
    fn new(now: Instant) -> Self {
        StreamReport {
            started: None,
            video: MediaTally::new(now),
            audio: MediaTally::new(now),
        }
    }

    /// Account for one RTP packet.
    fn record(&mut self, kind: MediaKind, now: Instant, marker: bool, keyframe: bool, bytes: u64) {
        self.started.get_or_insert(now);
        let tally = match kind {
            MediaKind::Video => &mut self.video,
            MediaKind::Audio => &mut self.audio,
        };
        tally.packets += 1;
        tally.bytes += bytes;
        // The marker bit ends a video frame and marks the last audio packet of
        // a talkspurt, which is close enough to count frames and packets.
        if marker {
            tally.frames += 1;
        }
        if keyframe {
            tally.keyframes += 1;
            tally.keyframes_total += 1;
            tally.last_keyframe = Some(now);
        }
    }

    /// Print the periodic report and start a new interval.
    ///
    /// Silence is reported too: a publisher that stopped sending is the one
    /// thing a line full of zeroes says best.
    fn log(&mut self, now: Instant, viewers: usize) {
        let Some(started) = self.started else {
            return;
        };
        let video = &self.video;
        let audio = &self.audio;
        if !video.active() && !audio.active() {
            return;
        }
        let secs = now.duration_since(video.since).as_secs_f64().max(0.001);
        let mut line = format!(
            "whip {elapsed}: ",
            elapsed = human_duration(now.duration_since(started))
        );
        if video.active() {
            let fps = video.frames as f64 / secs;
            let since_kf = video
                .last_keyframe
                .map(|at| format!("{:.1}s ago", now.duration_since(at).as_secs_f64()))
                .unwrap_or_else(|| "none yet".to_string());
            line.push_str(&format!(
                "video {packets} pkt {video} kbit/s {fps:.1} fps, \
                 {keyframes} keyframe(s) in this interval, last {since_kf}, \
                 {total} in total",
                video = video.kbit_per_second(now).round(),
                packets = video.packets,
                keyframes = video.keyframes,
                total = video.keyframes_total,
            ));
        } else {
            line.push_str("no video in this interval");
        }
        if audio.active() {
            line.push_str(&format!(
                ", audio {packets} pkt {audio} kbit/s",
                packets = audio.packets,
                audio = audio.kbit_per_second(now).round(),
            ));
        }
        line.push_str(&format!(", {viewers} viewer(s)"));
        eprintln!("{line}");
        self.video.restart(now);
        self.audio.restart(now);
    }

    /// Print the final summary of the session.
    fn log_final(&self, now: Instant) {
        let Some(started) = self.started else {
            eprintln!("whip input closed without ever receiving media");
            return;
        };
        let secs = now.duration_since(started).as_secs_f64().max(0.001);
        let bytes = self.video.bytes + self.audio.bytes;
        let kbit = (bytes as f64 * 8.0 / secs / 1000.0).round();
        eprintln!(
            "whip input closed after {elapsed}: {kbit} kbit/s average of \
             {bytes} bytes, video {packets} packet(s) with {keyframes} keyframe(s)",
            elapsed = human_duration(now.duration_since(started)),
            keyframes = self.video.keyframes_total,
            packets = self.video.packets,
        );
    }
}

/// Format a duration as `42s`, `7m13s` or `1h02m03s`.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Whether errors when sending to a local UDP socket are transient.
///
/// A player that is not running yet, or that just quit, is normal. On Linux a
/// previous `send` to a closed port shows up as `ConnectionRefused` on the next
/// operation, on Windows as `ConnectionReset`.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::NotConnected
    )
}

/// The media kind of a str0m media.
fn kind_of(kind: Str0mMediaKind) -> MediaKind {
    match kind {
        Str0mMediaKind::Video => MediaKind::Video,
        Str0mMediaKind::Audio => MediaKind::Audio,
    }
}

/// A message from the media session to exactly one viewer.
#[derive(Debug)]
enum Frame {
    /// The session header, so the viewer can (re)write its SDP.
    Header(Bytes),
    /// The next media packet is the start of a keyframe.
    Epoch,
    /// A video RTP packet.
    Video(Bytes),
    /// An audio RTP packet.
    Audio(Bytes),
}

/// What the viewer acceptor tells the media session.
#[derive(Debug)]
enum ViewerEvent {
    /// A viewer dialed in and announced its local RTP port.
    Joined {
        /// Identifier of this viewer.
        id: u64,
        /// The local RTP port the viewer plays out on.
        port: u16,
        /// Where to send the media of this viewer.
        tx: mpsc::Sender<Frame>,
    },
    /// A viewer asked for a keyframe, e.g. because it just started its player
    /// or because it detected a gap.
    Keyframe {
        /// Identifier of the viewer.
        id: u64,
    },
    /// A viewer is gone.
    Left {
        /// Identifier of the viewer.
        id: u64,
    },
}

/// Identifier assigned to the next viewer.
static NEXT_VIEWER: AtomicU64 = AtomicU64::new(1);

/// Configuration of the WHIP host.
pub struct WhipConfig {
    /// The HTTP address to serve WHIP on.
    pub listen: SocketAddr,
    /// The bearer token OBS must present, if any.
    pub bearer_token: Option<String>,
    /// The address to bind the ICE socket on, if not derived from `listen`.
    pub ice_addr: Option<SocketAddr>,
    /// Whether to log periodic counters.
    pub stats: bool,
}

/// Listen for a WHIP stream from OBS and forward it to connecting viewers.
///
/// Runs three concurrent parts until one of them fails or control-c is pressed:
/// the WHIP HTTP server, the viewer acceptor on the iroh endpoint, and the
/// media session loop that owns the [`Rtc`].
pub async fn listen_whip(endpoint: Endpoint, cfg: WhipConfig) -> Result<()> {
    let ice = Arc::new(bind_ice(cfg.ice_addr, cfg.listen.ip()).await?);
    let ice_addr = ice
        .local_addr()
        .std_context("error getting the ice socket address")?;
    tracing::info!("webrtc ice candidate is udp://{ice_addr}");

    let (req_tx, req_rx) = mpsc::channel::<WhipRequest>(16);
    let (viewer_tx, viewer_rx) = mpsc::channel::<ViewerEvent>(32);

    let http = whip::serve(cfg.listen, cfg.bearer_token.clone(), req_tx);
    let accept = accept_viewers(endpoint.clone(), viewer_tx);
    let host = host_loop(ice, ice_addr, req_rx, viewer_rx, cfg.stats);

    let result = tokio::select! {
        res = http => res,
        res = accept => res,
        res = host => res,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("got ctrl-c, exiting");
            Ok(())
        }
    };
    tracing::info!("stopping whip input");
    endpoint.close().await;
    result
}

/// Bind the UDP socket that ICE/DTLS/SRTP runs on.
///
/// A loopback WHIP address gets a loopback socket, everything else gets the
/// address of the interface that routes to the internet, which is the address a
/// publisher on the local network can reach. Use `--ice-addr` to override that,
/// e.g. on a machine with several interfaces or a VPN.
async fn bind_ice(configured: Option<SocketAddr>, listen: IpAddr) -> Result<UdpSocket> {
    let bind = match configured {
        Some(addr) => addr,
        None if listen.is_loopback() => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        None => SocketAddr::from((primary_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED), 0)),
    };
    let socket = UdpSocket::bind(bind)
        .await
        .std_context(format!("error binding ice udp socket to {bind}"))?;
    // Maximize the kernel buffers so an instantaneous keyframe burst from the
    // publisher (OBS emits a 6 Mbps IDR as 300+ KB at once) does not overrun
    // the default SO_RCVBUF (~212 KB on Linux, 50 KB on macOS) and lose the
    // tail of the keyframe before str0m ever sees it. `let _ =` swallows an
    // error so a lower OS hard limit just clamps to the maximum allowed.
    let sock = socket2::SockRef::from(&socket);
    let _ = sock.set_recv_buffer_size(8 * 1024 * 1024);
    let _ = sock.set_send_buffer_size(8 * 1024 * 1024);
    Ok(socket)
}

/// The IPv4 address this machine would use to reach the internet.
///
/// A UDP `connect` does not send anything, it just makes the kernel pick the
/// source address of the route, which is exactly what we want to advertise as
/// our ICE candidate.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(("8.8.8.8", 53)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(addr) => Some(addr),
        IpAddr::V6(_) => None,
    }
}

/// Accept viewers on the endpoint until the endpoint is closed.
async fn accept_viewers(endpoint: Endpoint, tx: mpsc::Sender<ViewerEvent>) -> Result<()> {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            break;
        };
        let accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(e) => {
                tracing::warn!("error accepting connection: {e}");
                continue;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(cause) = handle_viewer(accepting, tx).await {
                tracing::warn!("error handling viewer: {cause}");
            }
        });
    }
    Ok(())
}

/// Handle one viewer connection.
///
/// The bidi stream is only used for the handshake and the port announcement, but
/// is kept open as a liveness signal while the media rides along on the same
/// connection as datagrams.
async fn handle_viewer(accepting: Accepting, tx: mpsc::Sender<ViewerEvent>) -> Result<()> {
    let conn = accepting.await.std_context("error accepting connection")?;
    let remote = conn.remote_id();
    tracing::info!("got viewer connection from {remote}");

    let max = match conn.max_datagram_size() {
        Some(max) => max,
        None => bail_any!("the viewer does not support quic datagrams"),
    };

    let (send, mut recv) = conn
        .accept_bi()
        .await
        .std_context("error accepting viewer stream")?;
    read_handshake(&mut recv).await?;
    let port = read_announcement(&mut recv).await?;
    tracing::info!("viewer {remote} plays out on udp://127.0.0.1:{port}");

    let id = NEXT_VIEWER.fetch_add(1, Ordering::Relaxed);
    let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(VIEWER_QUEUE);
    if tx
        .send(ViewerEvent::Joined {
            id,
            port,
            tx: frame_tx,
        })
        .await
        .is_err()
    {
        bail_any!("the media session is gone");
    }

    // The bidi stream is the liveness signal, dropping it here tells the viewer
    // that the host is gone.
    let _liveness = (send, recv);

    let counters = Arc::new(Counters::default());
    let forward = {
        let conn = conn.clone();
        let counters = counters.clone();
        tokio::spawn(async move {
            // The id of the next fragmented datagram.
            let mut frag_id: u16 = 0;
            while let Some(frame) = frame_rx.recv().await {
                let (tag, payload) = match frame {
                    Frame::Header(b) => (TAG_SESSION, b),
                    Frame::Epoch => (TAG_EPOCH, Bytes::new()),
                    Frame::Video(b) => (TAG_VIDEO, b),
                    Frame::Audio(b) => (TAG_AUDIO, b),
                };
                let mut buf = Vec::with_capacity(1 + payload.len());
                buf.push(tag);
                buf.extend_from_slice(&payload);
                // The limit can move with the path MTU estimate, so ask on
                // every packet rather than once.
                let max = conn.max_datagram_size().unwrap_or(max);
                if buf.len() > max {
                    // Too large for one datagram: fragment it instead of
                    // dropping it. A lost fragment is plain packet loss, the
                    // viewer's gap detection and keyframe gate recover from it.
                    let frags = match rtp::fragment_all(&buf, frag_id, max) {
                        Some(frags) => frags,
                        None => {
                            counters.record_dropped(buf.len(), max);
                            continue;
                        }
                    };
                    frag_id = frag_id.wrapping_add(1);
                    for frag in frags {
                        match conn.send_datagram(frag.into()) {
                            Ok(()) => {
                                counters.fragmented.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::debug!("cannot send fragment to viewer: {e}");
                                break;
                            }
                        }
                    }
                    continue;
                }
                match conn.send_datagram(buf.into()) {
                    Ok(()) => {
                        counters.forwarded.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(SendDatagramError::TooLarge) => {
                        counters.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::debug!("cannot send datagram to viewer: {e}");
                        break;
                    }
                }
            }
        })
    };

    let mut stats_tick = interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stats_tick.tick() => counters.log_periodic(),
            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(data) => {
                        if data.first() == Some(&TAG_KEYFRAME_REQ) {
                            counters.keyframes.fetch_add(1, Ordering::Relaxed);
                            if tx.send(ViewerEvent::Keyframe { id }).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("viewer {remote} datagram stream ended: {e}");
                        break;
                    }
                }
            }
        }
    }

    counters.log_final();
    forward.abort();
    let _ = tx.send(ViewerEvent::Left { id }).await;
    Ok(())
}

/// Read the `hello` handshake from a viewer.
async fn read_handshake(recv: &mut RecvStream) -> Result<()> {
    let mut buf = [0u8; crate::HANDSHAKE.len()];
    recv.read_exact(&mut buf).await.anyerr()?;
    ensure_any!(buf == crate::HANDSHAKE, "invalid handshake");
    tracing::debug!("handshake verified");
    Ok(())
}

/// Read the `rtp <port>` announcement line of a viewer.
async fn read_announcement(recv: &mut RecvStream) -> Result<u16> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = recv.read(&mut byte).await.anyerr()?;
        if read.unwrap_or(0) == 0 {
            bail_any!("viewer closed the stream before announcing its port");
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        ensure_any!(line.len() < 64, "viewer announcement is too long");
    }
    let text = String::from_utf8_lossy(&line).into_owned();
    let port = text
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .std_context(format!("invalid announcement line `{text}`"))?;
    tracing::debug!("viewer announcement: {text}");
    Ok(port)
}

/// One connected viewer.
struct Viewer {
    /// Identifier of the viewer.
    id: u64,
    /// Where to send the media of this viewer.
    tx: mpsc::Sender<Frame>,
    /// Whether the keyframe gate of this viewer is open.
    streaming: bool,
}

/// The host side of the media session.
///
/// One task owns the [`Rtc`] and everything it mutates, as required by str0m's
/// single mutation invariant: one input, then drain the output, then wait.
struct Host {
    /// The WebRTC session, renegotiated by every WHIP offer.
    rtc: Rtc,
    /// The ICE socket.
    ice: Arc<UdpSocket>,
    /// The address of the ICE socket, advertised as our ICE candidate.
    ice_addr: SocketAddr,
    /// The media the publisher offered.
    offer: Vec<OfferMedia>,
    /// What the viewers are told about the stream.
    header: SessionHeader,
    /// The last session header bytes sent to a viewer.
    last_header: Vec<u8>,
    /// The H.264 parameter sets seen in band.
    sets: ParameterSets,
    /// The connected viewers.
    viewers: Vec<Viewer>,
    /// Counter for the entity tags of the resources.
    etag_seq: u64,
    /// The mids of the live media.
    mids: Vec<(Mid, MediaKind)>,
    /// Counters for logging.
    counters: Arc<Counters>,
    /// Rolling media statistics for the periodic report.
    report: StreamReport,
    /// The media kinds we have seen a packet of, to log the milestone once.
    seen: Vec<MediaKind>,
    /// Whether to log periodic counters.
    stats: bool,
}

impl Host {
    fn new(ice: Arc<UdpSocket>, ice_addr: SocketAddr, stats: bool) -> Self {
        Host {
            rtc: new_rtc(),
            ice,
            ice_addr,
            offer: Vec::new(),
            header: SessionHeader {
                gen: 0,
                media: Vec::new(),
            },
            last_header: Vec::new(),
            sets: ParameterSets::default(),
            viewers: Vec::new(),
            etag_seq: 0,
            mids: Vec::new(),
            counters: Arc::new(Counters::default()),
            report: StreamReport::new(Instant::now()),
            seen: Vec::new(),
            stats,
        }
    }

    /// Handle a request from the WHIP HTTP server.
    fn on_whip(&mut self, req: WhipRequest) {
        match req {
            WhipRequest::Offer {
                sdp,
                via_put,
                reply,
            } => {
                let answer = self.negotiate(&sdp, via_put);
                if answer.is_err() {
                    tracing::warn!(
                        "rejecting a whip offer: {}",
                        answer.as_ref().err().unwrap_or(&String::new())
                    );
                }
                if reply.send(answer).is_err() {
                    tracing::debug!("the publisher hung up before the answer");
                }
            }
            WhipRequest::Trickle { sdp } => self.add_trickle(sdp),
            WhipRequest::Delete => {
                tracing::info!("whip session deleted");
                self.close_session();
            }
        }
    }

    /// Handle one event from the viewer acceptor.
    fn on_viewer_event(&mut self, event: ViewerEvent) {
        match event {
            ViewerEvent::Joined { id, port, tx } => {
                self.viewers.retain(|v| v.id != id);
                self.viewers.push(Viewer {
                    id,
                    tx,
                    streaming: false,
                });
                eprintln!(
                    "viewer {id} connected on udp://127.0.0.1:{port}, \
                     {count} viewer(s) now watching, waiting for a keyframe",
                    count = self.viewers.len()
                );
                tracing::info!(
                    "viewer {id} joined on udp://127.0.0.1:{port}, waiting for a keyframe"
                );
                self.refresh_header(true);
                self.request_keyframe();
            }
            ViewerEvent::Keyframe { id } => {
                // A viewer asks for a keyframe when it starts its player, when
                // media stalls, or when it sees a gap. If the viewer is already
                // streaming it has its SDP and parameter sets, so we must NOT
                // close the gate: on a lossy path nearly every frame has a gap,
                // and closing the gate on each one drops the whole stream until
                // the next IDR, which the player reports as thousands of missed
                // packets, jumping timestamps and a blackout between keyframes.
                // Keep forwarding and just nudge the encoder to refresh; the
                // player's jitter buffer and error concealment ride out the loss.
                let _ = id;
                self.request_keyframe();
            }
            ViewerEvent::Left { id } => {
                self.viewers.retain(|v| v.id != id);
                eprintln!(
                    "viewer {id} disconnected, {count} viewer(s) now watching",
                    count = self.viewers.len()
                );
                tracing::info!("viewer {id} left");
            }
        }
    }

    /// Accept an SDP offer from the publisher and answer it.
    ///
    /// Every offer starts a fresh session: a `POST` is a new resource and a
    /// `PUT` replaces the existing one, so the old ICE/DTLS state is dropped
    /// and the viewers are re-gated to wait for the next keyframe.
    fn negotiate(&mut self, offer: &str, via_put: bool) -> std::result::Result<Answer, String> {
        let parsed = sdp::parse_offer(offer);
        if parsed.iter().all(|m| m.payloads.is_empty()) {
            return Err("the offer contains no audio or video media".to_string());
        }

        let mut rtc = new_rtc();
        let candidate = Candidate::host(self.ice_addr, Protocol::Udp).map_err(|e| {
            format!(
                "cannot make an ice candidate for {addr}: {e}",
                addr = self.ice_addr
            )
        })?;
        rtc.add_local_candidate(candidate);
        let sdp_offer =
            SdpOffer::from_sdp_string(offer).map_err(|e| format!("cannot parse the offer: {e}"))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(sdp_offer)
            .map_err(|e| format!("cannot accept the offer: {e}"))?;
        let answer_sdp = answer.to_sdp_string();

        self.rtc = rtc;
        self.offer = parsed;
        self.mids.clear();
        self.sets = ParameterSets::default();
        self.header = SessionHeader {
            gen: 0,
            media: Vec::new(),
        };
        self.last_header = Vec::new();
        self.report = StreamReport::new(Instant::now());
        self.seen.clear();
        for viewer in &mut self.viewers {
            viewer.streaming = false;
        }
        self.etag_seq += 1;
        let etag = format!("dumbpipe-{}", self.etag_seq);
        tracing::info!(
            "accepted a whip {} with {} media line(s), etag {etag}",
            if via_put { "PUT" } else { "POST" },
            self.offer.len()
        );
        Ok(Answer {
            sdp: answer_sdp,
            etag,
        })
    }

    /// Add trickle ICE candidates from a `PATCH` body.
    fn add_trickle(&mut self, sdp: String) {
        let mut added = 0;
        for line in sdp.lines() {
            let Some(rest) = line.strip_prefix("a=candidate:") else {
                continue;
            };
            let Some(c) = parse_candidate(rest) else {
                tracing::debug!("cannot parse trickle candidate: {rest}");
                continue;
            };
            self.rtc.add_remote_candidate(c);
            added += 1;
        }
        tracing::debug!("added {added} trickle ice candidates");
    }

    /// Tear down the WebRTC session, keeping the viewers connected.
    ///
    /// The viewers stay, with their gate closed, so that a publisher that
    /// reconnects within seconds does not need them to redial.
    fn close_session(&mut self) {
        self.rtc.disconnect();
        self.offer.clear();
        self.mids.clear();
        self.sets = ParameterSets::default();
        for viewer in &mut self.viewers {
            viewer.streaming = false;
        }
    }

    /// The kind and codec of a payload type the publisher offered.
    fn payload_of(&self, pt: u8) -> Option<(MediaKind, &sdp::Payload)> {
        for media in &self.offer {
            for payload in &media.payloads {
                if payload.pt == pt {
                    return Some((media.kind, payload));
                }
            }
        }
        None
    }

    /// Rebuild the session header and send it to every viewer.
    ///
    /// With `force` the header goes out even when unchanged, which is what the
    /// periodic tick does: the header is the viewers' only description of the
    /// stream, and datagrams are unreliable.
    fn refresh_header(&mut self, force: bool) {
        let bytes = self.header.encode();
        if !force && bytes == self.last_header {
            return;
        }
        self.last_header = bytes.clone();
        self.send_all(Frame::Header(Bytes::from(bytes)));
    }

    fn send_all(&self, frame: Frame) {
        for viewer in &self.viewers {
            try_send(&viewer.tx, &frame, &self.counters);
        }
    }

    /// Forward one incoming str0m RTP packet.
    fn on_rtp(&mut self, packet: RtpPacket) {
        let info = RtpInfo {
            marker: packet.header.marker,
            pt: *packet.header.payload_type,
            seq: packet.header.sequence_number,
            timestamp: packet.header.timestamp,
            ssrc: *packet.header.ssrc,
        };
        self.handle_media(info, &packet.payload);
    }

    /// Forward one RTP packet to the viewers whose gate is open.
    fn handle_media(&mut self, info: RtpInfo, payload: &[u8]) {
        let pt = info.pt;
        let Some((kind, announced)) = self.payload_of(pt) else {
            // rtx, red, or a payload type that was never negotiated.
            self.counters.unwanted.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let codec = Codec::from_name(&announced.codec);
        // Own the announcement: the rest of this function mutates the session.
        let params = announced.clone();
        let data = Bytes::from(rtp::serialize_rtp(&info, payload));

        if codec == Codec::H264 {
            let before = self.sets.clone();
            rtp::scan_h264_parameter_sets(payload, &mut self.sets);
            if before != self.sets {
                // The parameter sets travel with the header, no generation bump.
                self.set_header_media(kind, pt, &params);
            }
        }

        // Correct the announced payload type if the publisher picked another one
        // than the first we guessed.
        let matches = self
            .header
            .media(kind)
            .is_some_and(|m| m.pt == pt && m.codec == params.codec);
        if !matches {
            self.header.gen += 1;
            self.set_header_media(kind, pt, &params);
        }

        let size = data.len() as u64;
        let keyframe = kind == MediaKind::Video && rtp::starts_keyframe(payload, codec);
        self.log_streaming(kind, &params, keyframe);
        self.report
            .record(kind, Instant::now(), info.marker, keyframe, size);

        if self.viewers.is_empty() {
            self.counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let frame = if kind == MediaKind::Video {
            Frame::Video(data)
        } else {
            Frame::Audio(data)
        };
        let mut opened = 0;
        for viewer in &mut self.viewers {
            if !viewer.streaming {
                if !keyframe {
                    continue;
                }
                viewer.streaming = true;
                opened += 1;
                // The viewer needs its SDP before it can start the player, so
                // send the header right before the epoch marker.
                try_send(
                    &viewer.tx,
                    &Frame::Header(Bytes::from(self.header.encode())),
                    &self.counters,
                );
            }
            // Tag every keyframe with an epoch, not just the one that opens the
            // gate: the viewer uses it to reset its sequence baseline (so an IDR
            // does not look like a gap) and to pace its keyframe requests, which
            // keeps a lossy path from poking the encoder with a PLI per frame.
            if keyframe {
                try_send(&viewer.tx, &Frame::Epoch, &self.counters);
            }
            try_send(&viewer.tx, &frame, &self.counters);
        }
        if opened > 0 {
            tracing::info!("opened the gate for {opened} viewer(s) at a keyframe");
        }
        let count = self.viewers.len() as u64;
        self.counters.forwarded.fetch_add(count, Ordering::Relaxed);
        self.counters
            .forwarded_bytes
            .fetch_add(frame_len(&frame) as u64 * count, Ordering::Relaxed);
    }

    /// Put or replace the session header entry of a media kind.
    fn set_header_media(&mut self, kind: MediaKind, pt: u8, payload: &sdp::Payload) {
        let media = MediaHeader {
            kind,
            pt,
            codec: payload.codec.clone(),
            clock: payload.clock,
            channels: if kind == MediaKind::Audio {
                payload.channels
            } else {
                1
            },
            fmtp: payload.fmtp.clone(),
            sets: self.sets.clone(),
        };
        match self.header.media.iter_mut().find(|m| m.kind == kind) {
            Some(existing) => *existing = media,
            None => self.header.media.push(media),
        }
    }

    /// Announce, once per media kind, that the publisher is sending it.
    ///
    /// The WHIP handshake answers with an SDP before a single packet flows, so
    /// this is the first moment we can honestly say the stream works. The line
    /// names the codec the publisher actually chose, which is the thing an
    /// operator staring at a black player wants to see confirmed.
    fn log_streaming(&mut self, kind: MediaKind, params: &sdp::Payload, keyframe: bool) {
        if self.seen.contains(&kind) {
            return;
        }
        self.seen.push(kind);
        let viewers = self.viewers.len();
        if kind == MediaKind::Video {
            let gate = if keyframe {
                "streaming now".to_string()
            } else {
                format!("waiting for a keyframe to open the gate for {viewers} viewer(s)")
            };
            eprintln!(
                "whip streaming video: {codec}/{clock}, {gate}",
                codec = params.codec,
                clock = params.clock
            );
        } else {
            eprintln!(
                "whip streaming audio: {codec}/{clock}/{channels}",
                codec = params.codec,
                clock = params.clock,
                channels = params.channels
            );
        }
    }

    /// Print the periodic media report of the last interval.
    fn report_media(&mut self) {
        self.report.log(Instant::now(), self.viewers.len());
    }

    /// Ask the publisher for a keyframe.
    fn request_keyframe(&mut self) {
        let mut requested = false;
        for (mid, kind) in self.mids.clone() {
            if kind != MediaKind::Video {
                continue;
            }
            if let Some(rx) = self.rtc.direct_api().stream_rx_by_mid(mid, None) {
                rx.request_keyframe(KeyframeRequestKind::Pli);
                requested = true;
            }
        }
        if !requested {
            tracing::debug!("no video receive stream yet, cannot request a keyframe");
        }
    }

    /// Feed one datagram from the publisher into the WebRTC session.
    fn on_ice(&mut self, buf: &[u8], source: SocketAddr) {
        let Ok(receive) = Receive::new(Protocol::Udp, source, self.ice_addr, buf) else {
            tracing::trace!("ignoring {source} data that is not webrtc");
            return;
        };
        if let Err(e) = self
            .rtc
            .handle_input(Input::Receive(Instant::now(), receive))
        {
            tracing::debug!("webrtc input error: {e}");
        }
    }

    /// Drive the WebRTC session clock.
    fn on_timeout(&mut self) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
            tracing::debug!("webrtc timeout error: {e}");
        }
    }

    /// Handle one str0m event.
    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                eprintln!("whip publisher connected, ICE and DTLS up, waiting for media");
                tracing::info!("webrtc publisher connected");
            }
            Event::IceConnectionStateChange(state) => {
                tracing::debug!("ice connection state: {state:?}");
            }
            Event::MediaAdded(added) => {
                let kind = kind_of(added.kind);
                tracing::info!(
                    "publisher added {} media on mid {}",
                    kind.as_str(),
                    added.mid
                );
                self.mids.retain(|(mid, _)| *mid != added.mid);
                self.mids.push((added.mid, kind));
                self.header.gen += 1;
                self.refresh_header(true);
            }
            Event::MediaChanged(changed) => {
                tracing::info!("publisher changed media on mid {}", changed.mid);
                self.header.gen += 1;
                self.refresh_header(true);
            }
            Event::RtpPacket(packet) => self.on_rtp(packet),
            Event::StreamPaused(paused) => {
                tracing::info!(
                    "publisher stream on mid {} paused: {}",
                    paused.mid,
                    paused.paused
                );
            }
            Event::KeyframeRequest(_) => {
                // The publisher sends media, it does not ask us for keyframes.
            }
            Event::Closed => {
                tracing::info!("webrtc publisher closed the session");
                self.close_session();
            }
            _ => {}
        }
    }
}

/// Create a str0m session in RTP mode.
///
/// RTP mode is what makes the tunnel possible: str0m hands over the RTP packets
/// as they arrive instead of depacketizing them into frames, which is exactly
/// what a plain RTP player downstream wants.
fn new_rtc() -> Rtc {
    Rtc::builder().set_rtp_mode(true).build(Instant::now())
}

/// The byte size of a frame, for the counters.
fn frame_len(frame: &Frame) -> usize {
    match frame {
        Frame::Header(b) | Frame::Video(b) | Frame::Audio(b) => b.len(),
        Frame::Epoch => 0,
    }
}

/// Send a frame to a viewer, dropping it if the viewer is too slow.
///
/// A full queue is egress loss: count it as undeliverable so a slow or
/// overloaded viewer is visible in the stats instead of only in `trace!`.
fn try_send(tx: &mpsc::Sender<Frame>, frame: &Frame, counters: &Counters) {
    if tx.try_send(clone_frame(frame)).is_err() {
        counters.undeliverable.fetch_add(1, Ordering::Relaxed);
        tracing::trace!("viewer queue full, dropped a frame");
    }
}

/// Clone a frame cheaply; the payload is a `Bytes`.
fn clone_frame(frame: &Frame) -> Frame {
    match frame {
        Frame::Header(b) => Frame::Header(b.clone()),
        Frame::Epoch => Frame::Epoch,
        Frame::Video(b) => Frame::Video(b.clone()),
        Frame::Audio(b) => Frame::Audio(b.clone()),
    }
}

/// The media session loop: one task owns the `Rtc` and everything that mutates
/// it.
async fn host_loop(
    ice: Arc<UdpSocket>,
    ice_addr: SocketAddr,
    mut req_rx: mpsc::Receiver<WhipRequest>,
    mut viewer_rx: mpsc::Receiver<ViewerEvent>,
    stats: bool,
) -> Result<()> {
    let mut host = Host::new(ice.clone(), ice_addr, stats);
    let mut header_tick = interval(HEADER_INTERVAL);
    header_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stats_tick = interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report_tick = interval(STATS_INTERVAL);
    report_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buf = vec![0u8; ICE_BUF];

    loop {
        // Drain everything str0m wants to do before the next mutation.
        let deadline = drain(&mut host).await?;
        let sleep = sleep_until(tokio::time::Instant::from_std(deadline));

        tokio::select! {
            req = req_rx.recv() => {
                let Some(req) = req else { break };
                host.on_whip(req);
            }
            event = viewer_rx.recv() => {
                let Some(event) = event else { break };
                host.on_viewer_event(event);
            }
            res = ice.recv_from(&mut buf) => {
                let (len, source) = res.std_context("error receiving from the ice socket")?;
                host.on_ice(&buf[..len], source);
            }
            _ = sleep => host.on_timeout(),
            _ = header_tick.tick() => host.refresh_header(true),
            _ = stats_tick.tick(), if host.stats => host.counters.log_periodic(),
            _ = report_tick.tick() => host.report_media(),
        }
    }
    host.counters.log_final();
    host.report.log_final(Instant::now());
    Ok(())
}

/// Drain `poll_output` until str0m asks for the next timeout.
async fn drain(host: &mut Host) -> Result<Instant> {
    loop {
        match host.rtc.poll_output() {
            Ok(Output::Timeout(at)) => return Ok(at),
            Ok(Output::Transmit(t)) => {
                host.ice
                    .send_to(&t.contents, t.destination)
                    .await
                    .std_context("error sending on the ice socket")?;
            }
            Ok(Output::Event(event)) => host.on_event(event),
            Err(e) => bail_any!("webrtc session error: {e}"),
        }
    }
}

/// Parse one `a=candidate:` line of a trickle ICE fragment.
fn parse_candidate(rest: &str) -> Option<Candidate> {
    // `<foundation> <component> <transport> <priority> <ip> <port> typ <type> ...`
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 7 {
        return None;
    }
    let addr: SocketAddr = format!("{}:{}", fields[4], fields[5]).parse().ok()?;
    Candidate::host(addr, fields[2]).ok()
}

/// Configuration of the viewer.
pub struct ViewerConfig {
    /// The ticket of the host.
    pub addr: EndpointAddr,
    /// The ALPN to dial with.
    pub alpn: Vec<u8>,
    /// The local address to play out on.
    pub play: SocketAddr,
    /// Where to write the SDP file.
    pub sdp: Option<std::path::PathBuf>,
    /// Whether to write the default SDP file into the current folder rather than
    /// the system temp dir. Set for `--player none` and a bare `--sdp`.
    pub sdp_here: bool,
    /// The player to launch.
    pub player: Player,
    /// An explicit path to the player binary, for a player not on `PATH`.
    pub player_path: Option<std::path::PathBuf>,
    /// Whether to launch the player at all.
    pub no_launch: bool,
    /// How much jitter buffer to give the player, `None` for lowest latency.
    pub buffer: Option<Duration>,
    /// Whether to log periodic counters.
    pub stats: bool,
}

/// The local RTP ports the viewer plays out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayPorts {
    /// The video RTP port, always even.
    pub video: u16,
    /// The audio RTP port.
    pub audio: u16,
}

impl PlayPorts {
    /// Derive the ports from the requested address.
    ///
    /// RTP over UDP uses an even port with RTCP on the port above it, and a
    /// media player binds both for every media, so the two media need two pairs.
    pub fn new(requested: SocketAddr) -> Self {
        let video = if requested.port().is_multiple_of(2) {
            requested.port()
        } else {
            requested.port() + 1
        };
        PlayPorts {
            video,
            audio: video + 2,
        }
    }
}

/// A running player, so it can be stopped when the stream changes.
struct PlayerHandle {
    /// Send to make the player quit.
    stop: mpsc::Sender<()>,
}

/// A message from the viewer's QUIC read loop to its forwarding task.
///
/// The read loop never blocks on the localhost UDP send: it pushes packets to
/// a bounded channel and a dedicated task drains them to the player. A full
/// channel drops the packet, which reads as ordinary RTP loss and triggers a
/// throttled keyframe request, instead of the read loop parking on `send_to`
/// and dumping the QUIC backlog at the player in one burst.
enum ForwardMsg {
    /// A keyframe epoch: reset the gap-detection baseline in the forward task.
    Epoch,
    /// An RTP packet to forward: its player port, the bytes, and whether it is
    /// video (video drives gap detection and keyframe requests).
    Packet(u16, Vec<u8>, bool),
}

/// Connect to a WHIP host and play the stream out locally.
///
/// The viewer is a plain RTP forwarder: it announces the local video port, then
/// writes every forwarded packet to it, and writes the SDP the host describes the
/// stream with to a file a player can open.
pub async fn connect_whip(endpoint: Endpoint, cfg: ViewerConfig) -> Result<()> {
    let ports = PlayPorts::new(cfg.play);
    if ports.video != cfg.play.port() {
        tracing::info!(
            "rtp ports must be even, using udp://{}:{} instead of udp://{}",
            cfg.play.ip(),
            ports.video,
            cfg.play
        );
    }
    // An explicit `--sdp=path` wins; a bare `--sdp` or `--player none` drops
    // the default-named file in the current folder for the user to open; a
    // launched player keeps it in the temp dir.
    let sdp_path = sdp_path(cfg.sdp.clone(), cfg.sdp_here, ports.video);
    tracing::info!("writing the player description to {}", sdp_path.display());

    let video = Arc::new(
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .std_context("error binding the local forwarding socket")?,
    );
    // A big send buffer absorbs the bursts the forward task cannot pace away,
    // so a dense reorder flush does not spill onto the wire into ffplay's
    // default-size receive buffer. `let _ =` clamps to the OS hard limit.
    let _ = socket2::SockRef::from(&*video).set_send_buffer_size(8 * 1024 * 1024);
    let counters = Arc::new(Counters::default());

    let mut player: Option<PlayerHandle> = None;
    let mut written: Option<String> = None;
    let mut launched = String::new();

    loop {
        tracing::info!("connecting to {}", cfg.addr.id);
        let conn = endpoint
            .connect(cfg.addr.clone(), &cfg.alpn)
            .await
            .std_context("error connecting to the host")?;
        tracing::info!("connected to {}", cfg.addr.id);

        // Pick the player's jitter buffer for this path. A local network stays
        // at the lowest latency; a long-distance path gets a buffer sized by
        // its round-trip time, otherwise the player's zero-buffer RTP receiver
        // drops every reordered packet and the log fills with `max delay
        // reached` and corrupt pictures.
        let rtt = conn
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.rtt());
        let buffer = auto_buffer(cfg.buffer, rtt);
        if cfg.buffer.is_none() && buffer.is_some() {
            eprintln!(
                "long-distance path (rtt {} ms), buffering {} ms",
                rtt.unwrap_or_default().as_millis(),
                buffer.unwrap_or_default().as_millis(),
            );
        }

        let (mut send, recv) = conn
            .open_bi()
            .await
            .std_context("error opening stream to the host")?;
        write_handshake(&mut send).await?;
        send.write_all(format!("rtp {}\n", ports.video).as_bytes())
            .await
            .anyerr()?;
        // keep the stream open as a liveness signal
        let _liveness = (send, recv);

        let (kf_tx, mut kf_rx) = mpsc::channel::<()>(8);
        let (exit_tx, mut exit_rx) = mpsc::channel::<()>(4);

        let mut header: Option<SessionHeader> = None;
        let mut forwarding = false;
        // Datagrams the host had to fragment, reassembled before they are
        // forwarded to the player.
        let mut reassembler = rtp::Reassembler::default();
        // QUIC datagrams are unordered, so put each media back into RTP
        // sequence order before the player sees it; otherwise ffplay reads the
        // reordering as loss and corrupts the picture. Video and audio have
        // separate sequence spaces, so each gets its own buffer. The reorder
        // window is sized from `--buffer` so the two reorder stages stay in
        // tandem: a bigger player buffer lets us wait a little longer for a
        // reordered packet, but never long enough to stall on a lost one.
        let reorder_timeout = reorder_timeout_for(cfg.buffer);
        let mut video_order = Reorder::with_timeout(reorder_timeout);
        let mut audio_order = Reorder::with_timeout(reorder_timeout);

        // Decouple the QUIC read loop from the localhost UDP send. The read
        // loop pushes packets onto this bounded channel and never waits for the
        // player; a dedicated task drains it to ffplay. If the player hiccups
        // the channel fills and packets drop (counted as undeliverable), which
        // reads as ordinary RTP loss and triggers a throttled keyframe request,
        // instead of the read loop parking on send_to and releasing the QUIC
        // backlog at the player in one burst.
        let (forward_tx, mut forward_rx) = mpsc::channel::<ForwardMsg>(VIEWER_QUEUE);
        let forward_video = video.clone();
        let forward_counters = counters.clone();
        let forward_kf_tx = kf_tx.clone();
        let forward_task = tokio::spawn(async move {
            // Gap detection and keyframe pacing live here, driven by the order
            // packets are actually written to the player.
            let mut last_seq: Option<u16> = None;
            let mut last_kf: Option<Instant> = None;
            // Pace local UDP writes: ffplay opens its RTP socket via an SDP file
            // where FFmpeg ignores -buffer_size, so it is bounded by the OS default
            // SO_RCVBUF (~50-212 KB). When the reorder stage flushes a held burst it
            // can dump >100 KB at once and overrun that socket, dropping packets.
            // A token bucket caps a burst to 64 KB and refills at 16 KB/ms (16 MB/s,
            // ~128 Mbps): zero latency in steady state (the bucket stays full far
            // above the 6 Mbps stream), but a dense flush is spread over a few ms so
            // the player can drain its buffer.
            const BURST_CAP: usize = 64 * 1024;
            const TOKEN_RATE: usize = 16 * 1024; // bytes per ms
            let mut tokens = BURST_CAP;
            let mut last_fill = tokio::time::Instant::now();
            while let Some(msg) = forward_rx.recv().await {
                match msg {
                    ForwardMsg::Epoch => {
                        // A keyframe just arrived: reset the baseline so the gap
                        // right after it is not mistaken for a loss.
                        last_seq = None;
                        last_kf = Some(Instant::now());
                    }
                    ForwardMsg::Packet(port, packet, is_video) => {
                        let required = packet.len().min(BURST_CAP);
                        loop {
                            let now = tokio::time::Instant::now();
                            let elapsed_micros = now.duration_since(last_fill).as_micros() as u64;
                            if elapsed_micros > 0 {
                                let added =
                                    ((elapsed_micros * (TOKEN_RATE as u64)) / 1000) as usize;
                                tokens = (tokens + added).min(BURST_CAP);
                                last_fill = now;
                            }
                            if tokens >= required {
                                tokens -= required;
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        forward(
                            &forward_video,
                            &packet,
                            port,
                            &forward_counters,
                            &mut last_seq,
                            &mut last_kf,
                            is_video,
                            &forward_kf_tx,
                        )
                        .await;
                    }
                }
            }
        });

        let mut last_packet = tokio::time::Instant::now();
        let mut wait_tick = interval(WAIT_INTERVAL);
        wait_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stats_tick = interval(STATS_INTERVAL);
        stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let result: Result<()> = loop {
            let silence = sleep_until(last_packet + STALL_INTERVAL);
            tokio::select! {
                datagram = conn.read_datagram() => {
                    let data = match datagram {
                        Ok(data) => data,
                        Err(e) => {
                            break Err(anyerr!("error reading datagrams from the host: {e}"));
                        }
                    };
                    // Reassemble fragmented datagrams and let the complete
                    // datagram fall through to the dispatch below.
                    let data = match data.first().copied() {
                        Some(TAG_FRAGMENT) => match reassembler.push(&data, Instant::now()) {
                            Some(full) => Bytes::from(full),
                            None => continue,
                        },
                        _ => data,
                    };
                    match data.first().copied() {
                        Some(TAG_SESSION) => {
                            let Some(decoded) = SessionHeader::decode(&data[1..]) else {
                                tracing::warn!("cannot decode the session header");
                                continue;
                            };
                            if header.as_ref() == Some(&decoded) {
                                continue;
                            }
                            let signature = media_signature(&decoded);
                            let renegotiated = signature != launched;
                            header = Some(decoded);
                            if let Some(text) = render(header.as_ref(), ports, &mut written) {
                                write_sdp(&sdp_path, &text).await?;
                            }
                            if renegotiated {
                                if let Some(player) = player.take() {
                                    let _ = player.stop.send(()).await;
                                }
                                launched = signature;
                                player =
                                    launch(&cfg, buffer, &sdp_path, ports.video, &kf_tx, &exit_tx)
                                        .await?;
                            }
                        }
                        Some(TAG_EPOCH) => {
                            if header.as_ref().is_some_and(|h| h.has_media()) {
                                if !forwarding {
                                    forwarding = true;
                                    // Only wipe the buffers when opening the gate.
                                    // Mid-stream epochs would delete valid fragments
                                    // and reordered packets of the new keyframe,
                                    // since QUIC datagrams are unordered.
                                    reassembler.clear();
                                    video_order.clear();
                                    audio_order.clear();
                                    tracing::info!("streaming from a keyframe");
                                }
                                // Tell the forward task to reset its gap baseline:
                                // a gap right after this keyframe is not a loss and
                                // must not poke the encoder.
                                if forward_tx.try_send(ForwardMsg::Epoch).is_err() {
                                    counters.undeliverable.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Some(tag @ (TAG_VIDEO | TAG_AUDIO)) if forwarding => {
                            let is_video = tag == TAG_VIDEO;
                            let port = if is_video { ports.video } else { ports.audio };
                            let packet = &data[1..];
                            // Reorder by RTP sequence number before handing the
                            // packet to the player, so the unordered datagram path
                            // does not look like packet loss to ffplay.
                            let order = if is_video { &mut video_order } else { &mut audio_order };
                            let emit = match rtp::parse_header(packet) {
                                Some((info, _)) => {
                                    order.push(info.seq, packet.to_vec(), Instant::now())
                                }
                                None => vec![packet.to_vec()],
                            };
                            for pkt in emit {
                                if forward_tx
                                    .try_send(ForwardMsg::Packet(port, pkt, is_video))
                                    .is_err()
                                {
                                    counters.undeliverable.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            last_packet = tokio::time::Instant::now();
                        }
                        _ => {}
                    }
                }
                Some(()) = kf_rx.recv() => {
                    send_datagram(&conn, TAG_KEYFRAME_REQ, &counters);
                }
                Some(()) = exit_rx.recv() => {
                    tracing::info!("the player exited, waiting for the next keyframe");
                    player = None;
                    launched = String::new();
                    forwarding = false;
                }
                _ = silence, if forwarding => {
                    tracing::debug!("no media for a while, asking for a keyframe");
                    send_datagram(&conn, TAG_KEYFRAME_REQ, &counters);
                    last_packet = tokio::time::Instant::now();
                }
                _ = wait_tick.tick() => {
                    if !forwarding {
                        tracing::info!("waiting for media from {}", cfg.addr.id);
                    }
                }
                _ = stats_tick.tick(), if cfg.stats => counters.log_periodic(),
            }
        };

        // Stop the forward task before closing the connection so it cannot leak
        // across a reconnect.
        forward_task.abort();
        counters.log_final();
        if let Some(player) = player.take() {
            let _ = player.stop.send(()).await;
        }
        conn.close(0u32.into(), b"viewer is leaving");
        result?;
        // The loop only exits the inner select on an error, so this is
        // unreachable, but the compiler wants the reconnect.
        sleep(RECONNECT_DELAY).await;
    }
}

/// Render the player SDP if the header describes a stream and changed.
fn render(
    header: Option<&SessionHeader>,
    ports: PlayPorts,
    written: &mut Option<String>,
) -> Option<String> {
    let header = header?;
    if !header.has_media() {
        return None;
    }
    let text = sdp::render_player_sdp(header, ports.video, ports.audio);
    if written.as_ref() == Some(&text) {
        return None;
    }
    *written = Some(text.clone());
    Some(text)
}

/// A signature of the media of a header, to detect a codec change.
fn media_signature(header: &SessionHeader) -> String {
    header
        .media
        .iter()
        .map(|m| format!("{}:{}:{}", m.kind.as_str(), m.pt, m.codec))
        .collect::<Vec<_>>()
        .join(",")
}

/// The jitter buffer to give the player.
///
/// An explicit `--buffer` always wins. Otherwise the path decides: a local
/// network stays at the lowest latency, a long-distance path gets four
/// round-trip times, clamped to 150-500 ms. That is the reorder window the
/// player waits for late packets; without it a path that jitters more than a
/// few milliseconds drops intact packets and shows corrupt pictures.
fn auto_buffer(configured: Option<Duration>, rtt: Option<Duration>) -> Option<Duration> {
    configured.or_else(|| {
        rtt.filter(|r| *r >= WAN_RTT)
            .map(|r| (r * 4).clamp(Duration::from_millis(150), Duration::from_millis(500)))
    })
}

/// Where the SDP file goes.
///
/// An explicit path wins; a directory gets the default name inside it. With no
/// usable path, `here` drops the file in the current folder so the user can
/// open it by hand (`--player none`, or a bare `--sdp`); otherwise it goes to
/// the system temp dir to keep the folder clean.
fn sdp_path(
    explicit: Option<std::path::PathBuf>,
    here: bool,
    video_port: u16,
) -> std::path::PathBuf {
    let name = format!("dumbpipe-{video_port}.sdp");
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return if p.is_dir() { p.join(name) } else { p };
    }
    let dir = if here {
        std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir())
    } else {
        std::env::temp_dir()
    };
    dir.join(name)
}

/// Write the SDP file.
async fn write_sdp(path: &std::path::Path, sdp: &str) -> Result<()> {
    tokio::fs::write(path, sdp)
        .await
        .with_std_context(|_| format!("error writing sdp file to {path:?}"))?;
    tracing::info!("wrote {}", path.display());
    Ok(())
}

/// Launch the player and ask the host for a keyframe once it is listening.
///
/// `buffer` is the jitter buffer for this path, see [`auto_buffer`].
/// Returns `None` when no player is to be launched.
async fn launch(
    cfg: &ViewerConfig,
    buffer: Option<Duration>,
    sdp_path: &std::path::Path,
    video_port: u16,
    kf_tx: &mpsc::Sender<()>,
    exit_tx: &mpsc::Sender<()>,
) -> Result<Option<PlayerHandle>> {
    if !cfg.player.launches() {
        // No player at all: tell the operator exactly where the file is and how
        // to open it, since we are leaving the watching to them.
        let path = sdp_path.display();
        eprintln!("saved the stream description to {path}");
        eprintln!("open it in a player to watch, e.g. 'mpv {path}' or 'vlc {path}'");
        return Ok(None);
    }
    if cfg.no_launch {
        tracing::info!(
            "not launching a player, play with: {}",
            cfg.player
                .command(sdp_path, buffer, cfg.player_path.as_deref())
                .join(" ")
        );
        return Ok(None);
    }
    let command = cfg
        .player
        .command(sdp_path, buffer, cfg.player_path.as_deref());
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .spawn()
        .with_std_context(|_| format!("error launching {}", command[0]))?;
    tracing::info!("launched {}", command.join(" "));

    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let exited = exit_tx.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = stop_rx.recv() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            status = child.wait() => {
                tracing::debug!("the player exited: {status:?}");
                let _ = exited.send(()).await;
            }
        }
    });

    wait_until_listening(video_port).await;
    let _ = kf_tx.try_send(());
    Ok(Some(PlayerHandle { stop: stop_tx }))
}

/// Wait until a player has bound the RTP port.
///
/// Until then there is nobody to receive the forwarded packets, and starting a
/// player takes longer than the keyframe we would send it.
async fn wait_until_listening(port: u16) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let started = Instant::now();
    while started.elapsed() < PLAYER_READY_TIMEOUT {
        // Probe by binding the port, but drop the probe socket before we sleep.
        // A socket bound in a `match` scrutinee lives until the end of the whole
        // `match`, so binding and then sleeping inside the arm would hog the very
        // port the player is trying to bind and make it fail to open it.
        let free = match UdpSocket::bind(addr).await {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => false,
            Err(e) => {
                tracing::debug!("cannot probe the player port: {e}");
                return;
            }
        };
        if !free {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    tracing::warn!("the player did not open udp://{port}, forwarding anyway");
}

/// Forward one RTP packet to the local player port.
///
/// A gap in the video sequence nudges the viewer to ask the host for a
/// keyframe, but `last_kf` is refreshed by every keyframe that arrives (the
/// host tags each one with an epoch), so a gap right after an IDR asks for
/// nothing. Only a sustained gap with no keyframe for [`KF_REQUEST_INTERVAL`]
/// sends a PLI: on a lossy path nearly every frame has a gap, and a PLI per gap
/// turns into a keyframe storm that amplifies the loss instead of recovering.
async fn forward(
    socket: &Arc<UdpSocket>,
    packet: &[u8],
    port: u16,
    counters: &Arc<Counters>,
    last_seq: &mut Option<u16>,
    last_kf: &mut Option<Instant>,
    video: bool,
    kf_tx: &mpsc::Sender<()>,
) {
    let dst = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match socket.send_to(packet, dst).await {
        Ok(_) => {
            counters.forwarded.fetch_add(1, Ordering::Relaxed);
            counters
                .forwarded_bytes
                .fetch_add(packet.len() as u64, Ordering::Relaxed);
        }
        Err(e) if is_transient(&e) => {
            counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            tracing::trace!("the player is not listening on {dst} yet");
        }
        Err(e) => {
            counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("error forwarding to {dst}: {e}");
        }
    }
    if !video {
        return;
    }
    let Some((info, _)) = rtp::parse_header(packet) else {
        return;
    };
    let gap = last_seq.is_some_and(|last| info.seq != last.wrapping_add(1));
    *last_seq = Some(info.seq);
    let throttled = last_kf.is_some_and(|at| at.elapsed() < KF_REQUEST_INTERVAL);
    if gap && !throttled {
        *last_kf = Some(Instant::now());
        tracing::debug!("video sequence gap at {}, asking for a keyframe", info.seq);
        let _ = kf_tx.try_send(());
    }
}

/// Send a tagged control datagram to the host.
fn send_datagram(conn: &Connection, tag: u8, counters: &Arc<Counters>) {
    match conn.send_datagram(Bytes::from(vec![tag])) {
        Ok(()) => {
            if tag == TAG_KEYFRAME_REQ {
                counters.keyframes.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => tracing::debug!("cannot send control datagram: {e}"),
    }
}

/// Write the handshake to the host.
async fn write_handshake(send: &mut SendStream) -> Result<()> {
    send.write_all(&crate::HANDSHAKE).await.anyerr()?;
    tracing::debug!("handshake sent");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp::render_player_sdp;

    fn offer() -> String {
        let fingerprint = "00:01:02:03:04:05:06:07:08:09:0A:0B:0C:0D:0E:0F:10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E:1F";
        format!(
            "v=0\r\n\
             o=- 4611731400498476682 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0 1\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtcp-mux\r\n\
             a=ice-ufrag:aBcD\r\n\
             a=ice-pwd:012345678901234567890123456789\r\n\
             a=fingerprint:sha-256 {fingerprint}\r\n\
             a=setup:actpass\r\n\
             a=mid:0\r\n\
             a=sendonly\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
             a=rtpmap:97 rtx/90000\r\n\
             a=fmtp:97 apt=96\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtcp-mux\r\n\
             a=ice-ufrag:aBcD\r\n\
             a=ice-pwd:012345678901234567890123456789\r\n\
             a=fingerprint:sha-256 {fingerprint}\r\n\
             a=setup:actpass\r\n\
             a=mid:1\r\n\
             a=sendonly\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=fmtp:111 minptime=10;useinbandfec=1\r\n"
        )
    }

    fn nal(ty: u8, body: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x60 | ty];
        buf.extend_from_slice(body);
        buf
    }

    fn stap(nals: &[&[u8]]) -> Vec<u8> {
        let mut buf = vec![24];
        for nal in nals {
            buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            buf.extend_from_slice(nal);
        }
        buf
    }

    fn info(pt: u8, seq: u16) -> RtpInfo {
        RtpInfo {
            marker: false,
            pt,
            seq,
            timestamp: 90_000,
            ssrc: 7,
        }
    }

    fn keyframe() -> Vec<u8> {
        let sps = nal(7, b"sps");
        let pps = nal(8, b"pps");
        stap(&[&sps, &pps, &nal(5, b"idr")])
    }

    async fn host() -> Host {
        let ice = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let ice_addr = ice.local_addr().unwrap();
        Host::new(ice, ice_addr, false)
    }

    fn viewer(host: &mut Host) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(16);
        host.on_viewer_event(ViewerEvent::Joined {
            id: 1,
            port: 5004,
            tx,
        });
        rx
    }

    fn text(frame: Frame) -> String {
        match frame {
            Frame::Header(b) => String::from_utf8(b.to_vec()).expect("utf8 header"),
            other => panic!("expected a header, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn whip_offer_is_answered() {
        let mut host = host().await;
        let answer = host.negotiate(&offer(), false).expect("an answer");
        assert!(answer.sdp.contains("m=video"), "{}", answer.sdp);
        assert!(answer.sdp.contains("a=recvonly"), "{}", answer.sdp);
        assert!(!answer.etag.is_empty());
        assert_eq!(host.offer.len(), 2);
        assert_eq!(host.payload_of(96).unwrap().0, MediaKind::Video);
        assert_eq!(host.payload_of(96).unwrap().1.codec, "H264");
        assert!(host.payload_of(97).is_none(), "rtx is never forwarded");
    }

    #[tokio::test]
    async fn bad_offer_is_rejected() {
        let mut host = host().await;
        let err = host.negotiate("v=0\r\n", false).unwrap_err();
        assert!(!err.is_empty());
        assert!(host.offer.is_empty());
    }

    #[tokio::test]
    async fn mid_gop_join_starts_at_a_keyframe() {
        let mut host = host().await;
        host.negotiate(&offer(), false).expect("an answer");
        let mut rx = viewer(&mut host);

        let header = text(rx.try_recv().expect("a header"));
        assert!(header.contains("gen="), "{header}");

        host.handle_media(info(96, 10), &nal(1, b"inter"));
        host.handle_media(info(96, 11), &nal(1, b"inter"));
        assert!(rx.try_recv().is_err(), "media must wait for a keyframe");

        let kf = keyframe();
        host.handle_media(info(96, 12), &kf);
        let header = text(rx.recv().await.expect("a header"));
        assert!(header.contains("pt=96"), "{header}");
        assert!(header.contains("media video"), "{header}");
        assert!(matches!(rx.recv().await, Some(Frame::Epoch)));
        assert!(matches!(rx.recv().await, Some(Frame::Video(_))));

        let sdp = render_player_sdp(&host.header, 5004, 5006);
        assert!(
            sdp.contains("sprop-parameter-sets="),
            "a mid-gop join needs the parameter sets: {sdp}"
        );

        host.handle_media(info(96, 13), &nal(1, b"inter"));
        assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));
    }

    #[tokio::test]
    async fn keyframe_request_keeps_the_stream_flowing() {
        // Once a viewer is streaming it has its SDP and parameter sets, so a
        // keyframe request (a gap on a lossy path) must NOT blackout the stream:
        // the gate stays open, inter frames keep flowing, and every keyframe is
        // epoch-tagged so the viewer can pace its requests.
        let mut host = host().await;
        host.negotiate(&offer(), false).expect("an answer");
        let mut rx = viewer(&mut host);
        rx.try_recv().expect("a header");

        host.handle_media(info(96, 1), &keyframe());
        assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
        assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
        assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));

        // A gap nudges the encoder, but the gate stays open: the next inter
        // frame is still forwarded, no blackout until the next IDR.
        host.on_viewer_event(ViewerEvent::Keyframe { id: 1 });
        host.handle_media(info(96, 2), &nal(1, b"inter"));
        assert!(
            matches!(rx.try_recv(), Ok(Frame::Video(_))),
            "the gate stays open after a keyframe request"
        );

        // The next keyframe is epoch-tagged (no header, the viewer has it) so
        // the viewer resets its sequence baseline there.
        host.handle_media(info(96, 3), &keyframe());
        assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
        assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));
    }

    #[tokio::test]
    async fn audio_and_video_are_tagged_apart() {
        let mut host = host().await;
        host.negotiate(&offer(), false).expect("an answer");
        let mut rx = viewer(&mut host);
        rx.try_recv().expect("a header");

        host.handle_media(info(96, 1), &keyframe());
        assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
        assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
        assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));

        host.handle_media(info(111, 1), b"opus-frame");
        assert!(matches!(rx.try_recv(), Ok(Frame::Audio(_))));

        host.handle_media(info(97, 2), b"rtx");
        assert!(rx.try_recv().is_err());
        assert_eq!(host.counters.get(&host.counters.unwanted), 1);
    }

    #[tokio::test]
    async fn viewers_leave_and_rejoin() {
        let mut host = host().await;
        host.negotiate(&offer(), false).expect("an answer");
        let rx = viewer(&mut host);
        assert_eq!(host.viewers.len(), 1);

        host.handle_media(info(96, 1), &keyframe());
        host.on_viewer_event(ViewerEvent::Left { id: 1 });
        assert!(host.viewers.is_empty());

        host.handle_media(info(96, 2), &nal(1, b"inter"));
        assert_eq!(host.counters.get(&host.counters.undeliverable), 1);
        drop(rx);

        let mut rx = viewer(&mut host);
        rx.try_recv().expect("a header");
        host.handle_media(info(96, 3), &nal(1, b"inter"));
        assert!(rx.try_recv().is_err());
        host.handle_media(info(96, 4), &keyframe());
        assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
    }

    #[test]
    fn auto_buffer_follows_the_path_rtt() {
        // An explicit buffer always wins, whatever the path says.
        let explicit = Duration::from_millis(800);
        assert_eq!(
            auto_buffer(Some(explicit), Some(Duration::from_secs(1))),
            Some(explicit)
        );
        assert_eq!(auto_buffer(Some(explicit), None), Some(explicit));
        // A local path stays at the lowest latency.
        assert_eq!(auto_buffer(None, None), None);
        assert_eq!(auto_buffer(None, Some(Duration::from_millis(2))), None);
        assert_eq!(auto_buffer(None, Some(Duration::from_millis(49))), None);
        // A long-distance path gets four round-trips, clamped to 150-500 ms.
        assert_eq!(
            auto_buffer(None, Some(Duration::from_millis(50))),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            auto_buffer(None, Some(Duration::from_millis(80))),
            Some(Duration::from_millis(320))
        );
        assert_eq!(
            auto_buffer(None, Some(Duration::from_millis(120))),
            Some(Duration::from_millis(480))
        );
        assert_eq!(
            auto_buffer(None, Some(Duration::from_millis(300))),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn play_ports_are_even_pairs() {
        let ports = PlayPorts::new("127.0.0.1:5004".parse().unwrap());
        assert_eq!((ports.video, ports.audio), (5004, 5006));

        let ports = PlayPorts::new("127.0.0.1:5005".parse().unwrap());
        assert_eq!((ports.video, ports.audio), (5006, 5008));
    }

    #[test]
    fn player_commands_are_low_latency() {
        let path = Path::new("/tmp/x.sdp");
        let mpv = Player::Mpv.command(path, None, None);
        assert_eq!(mpv[0], "mpv");
        assert!(mpv.contains(&"--profile=low-latency".to_string()));
        assert!(mpv.contains(&"--no-resume-playback".to_string()));
        assert!(mpv.iter().any(|a| a.contains("max_delay=0")));
        assert!(mpv.iter().any(|a| a.contains("reorder_queue_size=0")));
        assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");
        assert!(Player::Vlc
            .command(path, None, None)
            .contains(&"--network-caching=100".to_string()));
        let ffplay = Player::Ffplay.command(path, None, None);
        assert!(ffplay.contains(&"nobuffer".to_string()));
        assert!(ffplay.iter().any(|a| a == "0"));
        assert!(!Player::None.launches());
        assert!(Player::Mpv.launches());
    }

    #[test]
    fn a_player_path_replaces_the_binary_but_keeps_the_flags() {
        let path = Path::new("/tmp/x.sdp");
        let bin = Path::new("/opt/mpv");
        let mpv = Player::Mpv.command(path, None, Some(bin));
        assert_eq!(mpv[0], "/opt/mpv");
        assert!(mpv.contains(&"--profile=low-latency".to_string()));
        assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");
        // The path is used whatever the player, with that player's flags.
        let ffplay = Player::Ffplay.command(path, Some(Duration::from_millis(300)), Some(bin));
        assert_eq!(ffplay[0], "/opt/mpv");
        assert!(ffplay.contains(&"+discardcorrupt".to_string()));
    }

    #[test]
    fn a_buffer_switches_the_player_out_of_zero_buffering() {
        let path = Path::new("/tmp/x.sdp");
        let ms = Duration::from_millis(300);
        let mpv = Player::Mpv.command(path, Some(ms), None);
        assert!(!mpv.contains(&"--profile=low-latency".to_string()));
        assert!(!mpv.contains(&"--no-cache".to_string()));
        assert!(mpv.contains(&"--cache=yes".to_string()));
        assert!(mpv.contains(&"--no-resume-playback".to_string()));
        assert!(mpv.iter().any(|a| a.contains("max_delay=300000")));
        assert!(mpv.iter().any(|a| a.contains("reorder_queue_size=32768")));
        assert!(mpv.iter().any(|a| a.contains("buffer_size=4194304")));

        let ffplay = Player::Ffplay.command(path, Some(ms), None);
        assert!(!ffplay.iter().any(|a| a == "nobuffer"));
        assert!(ffplay.contains(&"+discardcorrupt".to_string()));
        assert!(ffplay.iter().any(|a| a == "300000"));
        assert!(ffplay.iter().any(|a| a == "32768"));

        let vlc = Player::Vlc.command(path, Some(ms), None);
        assert!(vlc.contains(&"--network-caching=300".to_string()));
    }

    #[test]
    fn sdp_path_honours_explicit_here_and_default() {
        // A named file wins as-is.
        let named = sdp_path(Some("/tmp/mine.sdp".into()), false, 5004);
        assert_eq!(named, std::path::Path::new("/tmp/mine.sdp"));
        // A bare `--sdp`, or no player, drops the default name in the cwd.
        let here = sdp_path(None, true, 5004);
        assert!(here.starts_with(std::env::current_dir().unwrap()));
        assert_eq!(
            here.file_name().unwrap().to_str().unwrap(),
            "dumbpipe-5004.sdp"
        );
        // A launched player with no path keeps it in the temp dir.
        let tmp = sdp_path(None, false, 5004);
        assert!(tmp.starts_with(std::env::temp_dir()));
        assert!(!tmp.starts_with(std::env::current_dir().unwrap()));
    }

    #[test]
    fn trickle_candidates_are_parsed() {
        let c =
            parse_candidate("1 1 udp 2130706431 192.168.1.10 50000 typ host").expect("candidate");
        assert_eq!(c.addr().ip().to_string(), "192.168.1.10");
        assert!(parse_candidate("nonsense").is_none());
    }

    #[tokio::test]
    async fn forward_writes_rtp_and_asks_for_a_keyframe_on_a_gap() {
        let player = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = player.local_addr().unwrap().port();
        let sender = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let counters = Arc::new(Counters::default());
        let (kf_tx, mut kf_rx) = mpsc::channel(8);
        let mut last = None;
        let mut last_kf = None;

        let packet = |seq: u16| rtp::serialize_rtp(&info(96, seq), b"frame");

        forward(
            &sender,
            &packet(10),
            port,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;
        forward(
            &sender,
            &packet(11),
            port,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;
        assert_eq!(counters.forwarded.load(Ordering::Relaxed), 2);
        assert_eq!(counters.undeliverable.load(Ordering::Relaxed), 0);
        assert!(kf_rx.try_recv().is_err(), "no gap, no request");

        forward(
            &sender,
            &packet(20),
            port,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;
        assert_eq!(kf_rx.try_recv(), Ok(()), "a gap asks for a keyframe");

        // a second gap within the throttle interval does not ask again: on a
        // lossy path that would be a keyframe storm
        forward(
            &sender,
            &packet(30),
            port,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;
        assert!(kf_rx.try_recv().is_err(), "the second gap is throttled");

        // audio is not a video sequence, its gaps mean nothing here
        forward(
            &sender,
            &packet(40),
            port,
            &counters,
            &mut last,
            &mut last_kf,
            false,
            &kf_tx,
        )
        .await;
        assert!(kf_rx.try_recv().is_err(), "audio gaps are not video gaps");

        // the player must see exactly the bytes we were fed
        let mut buf = [0u8; 1024];
        for seq in [10u16, 11, 20, 30, 40] {
            let (n, _) = player.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], &packet(seq)[..], "packet {seq}");
        }
    }

    #[tokio::test]
    async fn forward_survives_a_player_that_is_not_listening() {
        // Nothing is bound on port 1. Whether the kernel reports the packet as
        // undeliverable depends on it having seen the ICMP error before, so
        // this asserts the invariant, not the split: the packet is accounted
        // for and the tunnel keeps going.
        let sender = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let counters = Arc::new(Counters::default());
        let (kf_tx, mut kf_rx) = mpsc::channel(8);
        let mut last = None;
        let mut last_kf = None;

        forward(
            &sender,
            &rtp::serialize_rtp(&info(96, 1), b"frame"),
            1,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;

        let forwarded = counters.forwarded.load(Ordering::Relaxed);
        let undeliverable = counters.undeliverable.load(Ordering::Relaxed);
        assert_eq!(forwarded + undeliverable, 1, "accounted for exactly once");
        assert!(kf_rx.try_recv().is_err(), "the first packet is no gap");

        forward(
            &sender,
            &rtp::serialize_rtp(&info(96, 9), b"frame"),
            1,
            &counters,
            &mut last,
            &mut last_kf,
            true,
            &kf_tx,
        )
        .await;
        assert_eq!(
            kf_rx.try_recv(),
            Ok(()),
            "a gap is a gap whether or not it was delivered"
        );
    }

    /// The whole ingest path against a real WebRTC peer.
    ///
    /// A second str0m instance does what OBS does: it offers H.264, completes
    /// ICE and DTLS on loopback, and writes one RTP keyframe. The host has to
    /// answer, decrypt, and hand the packet to the viewer behind the keyframe
    /// gate. This is the only test that exercises ICE, DTLS and SRTP at all;
    /// everything else drives the session through its own seams.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_publisher_reaches_a_viewer() {
        use str0m::{change::SdpAnswer, media::Direction, rtp::RtpWrite};

        let ice = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let ice_addr = ice.local_addr().unwrap();
        let mut host = Host::new(ice.clone(), ice_addr, false);
        let (tx, mut rx) = mpsc::channel(256);
        host.on_viewer_event(ViewerEvent::Joined {
            id: 1,
            port: 5004,
            tx,
        });

        let pub_ice = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let pub_addr = pub_ice.local_addr().unwrap();
        let mut pub_rtc = Rtc::builder()
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_h264(true)
            .build(Instant::now());
        pub_rtc.add_local_candidate(
            Candidate::host(pub_addr, Protocol::Udp).expect("a host candidate"),
        );

        let mut api = pub_rtc.sdp_api();
        let mid = api.add_media(Str0mMediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = api.apply().expect("a change");

        let answer = host
            .negotiate(&offer.to_sdp_string(), false)
            .expect("an answer");
        let answer = SdpAnswer::from_sdp_string(&answer.sdp).expect("a parseable answer");
        pub_rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("accepting the answer");

        // the payload type the publisher offered and we accepted
        let video = host
            .offer
            .iter()
            .find(|m| m.kind == MediaKind::Video)
            .expect("video in the offer");
        let pt = video.payloads.first().expect("a video payload type");
        assert_eq!(pt.codec, "H264");
        let pt = pt.pt;

        let mut host_buf = vec![0u8; ICE_BUF];
        let mut pub_buf = vec![0u8; ICE_BUF];
        let keyframe = keyframe();
        let mut frames: Vec<Frame> = Vec::new();
        let mut connected = false;
        let mut written = false;
        let started = Instant::now();

        loop {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the session did not come up, frames so far: {frames:?}"
            );

            // Drain both engines to their timeout before the next mutation, as
            // str0m's single mutation invariant demands.
            let host_deadline = loop {
                match host.rtc.poll_output().expect("host output") {
                    Output::Timeout(at) => break at,
                    Output::Transmit(t) => {
                        ice.send_to(&t.contents, t.destination).await.unwrap();
                    }
                    Output::Event(event) => {
                        if matches!(event, Event::Connected) {
                            connected = true;
                        }
                        host.on_event(event);
                    }
                }
            };
            let pub_deadline = loop {
                match pub_rtc.poll_output().expect("publisher output") {
                    Output::Timeout(at) => break at,
                    Output::Transmit(t) => {
                        pub_ice.send_to(&t.contents, t.destination).await.unwrap();
                    }
                    Output::Event(_) => {}
                }
            };

            if connected && !written {
                let mut direct = pub_rtc.direct_api();
                let stream = direct.stream_tx_by_mid(mid, None).expect("a send stream");
                stream.write_rtp(
                    RtpWrite::new(
                        pt.into(),
                        1000u64.into(),
                        90_000,
                        Instant::now(),
                        keyframe.clone(),
                    )
                    .marker(true),
                );
                written = true;
            }

            while let Ok(frame) = rx.try_recv() {
                frames.push(frame);
            }
            if written && frames.iter().any(|f| matches!(f, Frame::Video(_))) {
                break;
            }

            tokio::select! {
                res = ice.recv_from(&mut host_buf) => {
                    if let Ok((len, source)) = res {
                        host.on_ice(&host_buf[..len], source);
                    }
                }
                res = pub_ice.recv_from(&mut pub_buf) => {
                    if let Ok((len, source)) = res {
                        let contents = (&pub_buf[..len]).try_into().expect("a packet");
                        pub_rtc
                            .handle_input(Input::Receive(
                                Instant::now(),
                                Receive {
                                    proto: Protocol::Udp,
                                    source,
                                    destination: pub_addr,
                                    contents,
                                },
                            ))
                            .expect("publisher input");
                    }
                }
                _ = sleep_until(tokio::time::Instant::from_std(host_deadline)) => host.on_timeout(),
                _ = sleep_until(tokio::time::Instant::from_std(pub_deadline)) => {
                    pub_rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .expect("publisher timeout");
                }
            }
        }

        let kinds: Vec<&str> = frames
            .iter()
            .map(|f| match f {
                Frame::Header(_) => "header",
                Frame::Epoch => "epoch",
                Frame::Video(_) => "video",
                Frame::Audio(_) => "audio",
            })
            .collect();
        assert_eq!(kinds.first().copied(), Some("header"), "{kinds:?}");
        let epoch = kinds.iter().position(|k| *k == "epoch").expect("an epoch");
        let video = kinds.iter().position(|k| *k == "video").expect("video");
        assert!(epoch < video, "the epoch precedes the keyframe: {kinds:?}");

        let payload = frames
            .iter()
            .find_map(|f| match f {
                Frame::Video(b) => Some(b.clone()),
                _ => None,
            })
            .expect("video");
        let (info, start) = rtp::parse_header(&payload).expect("an rtp header");
        assert_eq!(info.pt, pt, "the payload type is untouched");
        assert_eq!(info.seq, 1000, "the sequence number is untouched");
        assert!(info.marker, "the marker bit is untouched");
        assert_eq!(
            &payload[start..],
            &keyframe[..],
            "the payload survived ice, dtls and srtp"
        );

        // the header the viewer got before the keyframe must carry the codec
        // and the parameter sets it saw in band, otherwise its player cannot
        // start; the first header a viewer gets is the empty one it joined on
        let header = frames[..epoch]
            .iter()
            .rev()
            .find_map(|f| match f {
                Frame::Header(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .expect("a header before the keyframe");
        assert!(header.contains("H264"), "{header}");
        assert!(header.contains("video"), "{header}");
    }

    #[test]
    fn media_tally_counts_bitrate_frames_and_keyframes() {
        let start = Instant::now();
        let mut tally = MediaTally::new(start);
        // 1250 bytes over exactly one second is 10 kbit/s.
        tally.bytes = 1250;
        tally.packets = 5;
        let one_sec = (tally.kbit_per_second(start + Duration::from_secs(1)) - 10.0).abs();
        assert!(one_sec < 0.001, "bitrate was {one_sec} off");
        // A zero length interval must not divide by zero.
        assert_eq!(tally.kbit_per_second(start), 0.0);
        assert!(tally.active());
        tally.restart(start + Duration::from_secs(1));
        assert_eq!(tally.packets, 0);
        assert_eq!(tally.bytes, 0);
        assert!(!tally.active());
    }

    #[test]
    fn stream_report_log_resets_the_interval_but_keeps_totals() {
        let start = Instant::now();
        let mut report = StreamReport::new(start);
        // Nothing recorded yet: reporting is a no-op and must not panic.
        report.log(start + Duration::from_secs(5), 0);
        report.record(MediaKind::Video, start, true, true, 1000);
        report.record(MediaKind::Audio, start, true, false, 500);
        assert_eq!(report.video.packets, 1);
        assert_eq!(report.video.frames, 1);
        assert_eq!(report.video.keyframes, 1);
        assert_eq!(report.audio.packets, 1);
        report.log(start + Duration::from_secs(5), 3);
        // The per interval tallies reset, the running keyframe total does not.
        assert_eq!(report.video.packets, 0);
        assert_eq!(report.video.keyframes, 0);
        assert_eq!(report.video.keyframes_total, 1);
        assert_eq!(report.audio.packets, 0);
    }

    #[test]
    fn human_duration_reads_like_a_stopwatch() {
        assert_eq!(human_duration(Duration::from_secs(42)), "42s");
        assert_eq!(human_duration(Duration::from_secs(7 * 60 + 13)), "7m13s");
        assert_eq!(
            human_duration(Duration::from_secs(3600 + 2 * 60 + 3)),
            "1h02m03s"
        );
    }

    #[tokio::test]
    async fn streaming_is_announced_once_per_kind() {
        let mut h = host().await;
        h.negotiate(&offer(), false).expect("an answer");
        let video = sdp::Payload {
            pt: 96,
            codec: "H264".into(),
            clock: 90000,
            channels: 1,
            fmtp: String::new(),
        };
        h.log_streaming(MediaKind::Video, &video, false);
        h.log_streaming(MediaKind::Video, &video, true);
        assert_eq!(h.seen, vec![MediaKind::Video]);
        h.log_streaming(MediaKind::Audio, &video, false);
        assert_eq!(h.seen, vec![MediaKind::Video, MediaKind::Audio]);
    }

    #[tokio::test]
    async fn wait_until_listening_does_not_hog_the_port() {
        // Regression: the probe used to hold the RTP port across its sleep, so
        // the player it was waiting for could never bind it. The port must stay
        // bindable by the player while we wait, and the wait must end once the
        // player has it.
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let wait = tokio::spawn(wait_until_listening(port));

        let mut player = None;
        for _ in 0..40 {
            match UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(socket) => {
                    player = Some(socket);
                    break;
                }
                Err(_) => sleep(Duration::from_millis(25)).await,
            }
        }
        let _player = player.expect("the player never got the port it was waiting for");

        tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("wait_until_listening did not return after the player bound")
            .unwrap();
    }
}
