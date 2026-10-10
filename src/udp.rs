//! UDP tunneling over iroh QUIC datagrams.
//!
//! This module bridges a local UDP socket and the unreliable, unordered,
//! message-boundary-preserving QUIC datagrams (RFC 9221) of an iroh
//! `Connection`. Because datagrams are not subject to the head-of-line
//! blocking of QUIC streams, protocols that do their own retransmission and
//! timing, e.g. SRT, can be tunneled without interfering with themselves.
//!
//! There are two directions, mirroring the tcp subcommands:
//!
//! * `listen-udp` accepts connections on an endpoint and forwards incoming
//!   datagrams to a fixed local UDP address. Replies from that address are
//!   forwarded back over the same connection.
//! * `connect-udp` listens on a local UDP address and forwards everything it
//!   receives to the remote endpoint. Datagrams coming back are sent to
//!   whichever local address most recently sent a packet.
//!
//! Datagrams larger than the connection's `max_datagram_size()` are dropped
//! and counted, never fragmented. If that happens, lower the packet size on
//! the sending application (for SRT: `pkt_size` / `payloadsize` /
//! `SRTO_PAYLOADSIZE`).
//!
//! # Logging
//!
//! The amount of detail is driven by the `-v` count of the calling subcommand,
//! which is passed to [`bridge`] as the `stats` flag and reflected in the log
//! levels used here:
//!
//! * info: tunnel established, max datagram size, and the full counters when the
//!   tunnel closes.
//! * debug: periodic counters while the tunnel runs, plus every local socket
//!   bind and every non-fatal failure.
//! * trace: one line per datagram, in both directions, with size and address.

use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use bytes::Bytes;
use iroh::endpoint::{Connection, SendDatagramError};
use n0_error::{bail_any, ensure_any, Result, StdResultExt};
use tokio::net::UdpSocket;

/// The ALPN used by the udp subcommands.
///
/// Deliberately different from the stream ALPN `streampipe::ALPN`, so that a
/// udp connector can never be mistaken for a stream connector and vice versa.
pub const ALPN: &[u8] = b"STREAMPIPE_UDP_V0";

/// Size of the buffer used to receive from a UDP socket.
///
/// This is the largest possible UDP payload, so no packet can ever be
/// truncated.
const UDP_BUF: usize = 65535;

/// How often periodic counters are logged while a tunnel is running.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Whether errors when sending to or receiving from a local UDP socket should
/// be treated as transient.
///
/// UDP is connectionless, so an unreachable peer is a normal occurrence, not
/// an error. On Windows, an ICMP "port unreachable" caused by an earlier
/// `send_to` additionally shows up on the *next* socket operation as
/// `ConnectionReset`, and on Linux as `ConnectionRefused` for connected
/// sockets. Neither must tear down the tunnel.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::NotConnected
    )
}

/// Resolve a `host:port` string to a single [`SocketAddr`], preferring IPv4.
///
/// IPv4 is preferred so that `localhost:9000` matches applications that only
/// bind to `127.0.0.1`.
pub fn resolve(host: &str) -> Result<SocketAddr> {
    let addrs = host
        .to_socket_addrs()
        .std_context(format!("invalid host string {host}"))?
        .collect::<Vec<_>>();
    ensure_any!(!addrs.is_empty(), "could not resolve {host}");
    let mut addrs = addrs;
    addrs.sort_by_key(|a| a.is_ipv6());
    Ok(addrs[0])
}

/// Bind an ephemeral UDP socket in the same family as `target`.
///
/// Loopback targets get a loopback-only socket, so that nothing else on the
/// local network can inject packets into the tunnel (and Windows does not show
/// a firewall prompt for it).
pub async fn bind_for(target: SocketAddr) -> Result<UdpSocket> {
    let bind: &str = match (target.ip().is_loopback(), target.is_ipv4()) {
        (true, true) => "127.0.0.1:0",
        (true, false) => "[::1]:0",
        (false, true) => "0.0.0.0:0",
        (false, false) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind)
        .await
        .std_context(format!("error binding udp socket to {bind}"))?;
    tracing::debug!(
        "bound local udp socket {} for target {target}",
        socket
            .local_addr()
            .map_or_else(|e| format!("unknown ({e})"), |addr| addr.to_string(),)
    );
    Ok(socket)
}

/// Counters for a running udp bridge, used for logging.
#[derive(Debug, Default)]
struct Counters {
    /// Packets sent as QUIC datagrams.
    sent: AtomicU64,
    /// Bytes sent as QUIC datagrams.
    sent_bytes: AtomicU64,
    /// Datagrams forwarded to the local UDP socket.
    recv: AtomicU64,
    /// Bytes of datagrams forwarded to the local UDP socket.
    recv_bytes: AtomicU64,
    /// Packets dropped because they were too large for a QUIC datagram.
    dropped: AtomicU64,
    /// Datagrams that could not be delivered because the destination was not
    /// known yet.
    no_peer: AtomicU64,
    /// Datagrams that could not be delivered to the local UDP socket.
    local_failed: AtomicU64,
}

impl Counters {
    fn get(&self, counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// Log a periodic stats line, at debug level.
    ///
    /// Only called when the caller asked for stats, i.e. `-vv` or higher.
    fn log_periodic(&self, target: Option<SocketAddr>) {
        tracing::debug!(
            "udp tunnel stats ({}): {} packets / {} bytes sent, {} packets / {} bytes received, \
             {} oversized dropped, {} undeliverable ({} no peer, {} local failure)",
            target.map_or("connect side".to_string(), |target| format!("to {target}")),
            self.get(&self.sent),
            self.get(&self.sent_bytes),
            self.get(&self.recv),
            self.get(&self.recv_bytes),
            self.get(&self.dropped),
            self.get(&self.no_peer) + self.get(&self.local_failed),
            self.get(&self.no_peer),
            self.get(&self.local_failed),
        );
    }

    /// Log the final stats line when the tunnel closes, at info level.
    fn log_final(&self) {
        tracing::info!(
            "udp tunnel closed: {} packets / {} bytes sent, {} packets / {} bytes received, \
             {} oversized dropped, {} undeliverable ({} no peer, {} local failure)",
            self.get(&self.sent),
            self.get(&self.sent_bytes),
            self.get(&self.recv),
            self.get(&self.recv_bytes),
            self.get(&self.dropped),
            self.get(&self.no_peer) + self.get(&self.local_failed),
            self.get(&self.no_peer),
            self.get(&self.local_failed),
        );
    }

    /// Record an oversized packet that had to be dropped.
    fn record_dropped(&self, size: usize, max: usize) {
        let count = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // log the first one and then every 1000th, so a misconfigured sender
        // is visible without flooding the log.
        if count == 1 || count.is_multiple_of(1000) {
            tracing::warn!(
                "dropped oversized udp packet ({size} > {max} bytes), \
                 {count} dropped so far; lower the packet size of the sender",
            );
        }
    }
}

/// Shuttle datagrams between a UDP socket and a QUIC connection.
///
/// * `target = Some(addr)`: incoming datagrams are always sent to `addr`
///   (the `listen-udp` side).
/// * `target = None`: incoming datagrams are sent to whichever local address
///   last sent us a packet (the `connect-udp` side).
/// * `stats`: log periodic counters while running, and one line per datagram
///   if the log level allows it.
///
/// Returns when either side fails, when the connection is closed, or when the
/// returned future is dropped.
pub async fn bridge(
    conn: Connection,
    sock: Arc<UdpSocket>,
    target: Option<SocketAddr>,
    stats: bool,
) -> Result<()> {
    // The peer must have announced datagram support during the handshake.
    match conn.max_datagram_size() {
        Some(size) => tracing::info!("quic datagrams enabled, max payload {size} bytes"),
        None => bail_any!("connection does not support quic datagrams"),
    }

    let counters = Arc::new(Counters::default());
    // Whether we have to learn the destination from incoming packets.
    let learn = target.is_none();
    let peer = Arc::new(Mutex::new(target));

    let local_addr = sock
        .local_addr()
        .map_or_else(|_| "unknown".to_string(), |addr| addr.to_string());
    tracing::debug!("udp bridge running, local socket {local_addr}");

    // Periodically report what the tunnel is doing, or never fire if the user
    // did not ask for stats.
    let counters2 = counters.clone();
    let stats_loop = async move {
        if !stats {
            std::future::pending::<()>().await;
        }
        loop {
            tokio::time::sleep(STATS_INTERVAL).await;
            counters2.log_periodic(target);
        }
    };

    let result = tokio::select! {
        res = udp_to_quic(&conn, &sock, &peer, learn, &counters) => res,
        res = quic_to_udp(&conn, &sock, &peer, &counters) => res,
        _ = stats_loop => unreachable_err(),
        _ = conn.closed() => Ok(()),
    };

    counters.log_final();
    result
}

/// Unreachable: the stats loop never returns `Ok`.
///
/// Exists so the `select!` in [`bridge`] can have a stats arm that is
/// conditionally disabled without changing the arm types.
fn unreachable_err() -> Result<()> {
    bail_any!("stats loop terminated unexpectedly")
}

/// Forward packets from the local UDP socket to the connection as datagrams.
async fn udp_to_quic(
    conn: &Connection,
    sock: &UdpSocket,
    peer: &Mutex<Option<SocketAddr>>,
    learn: bool,
    counters: &Counters,
) -> Result<()> {
    let mut buf = vec![0u8; UDP_BUF];
    loop {
        let (len, src) = match sock.recv_from(&mut buf).await {
            Ok((len, src)) => (len, src),
            Err(e) if is_transient(&e) => {
                tracing::debug!("transient error receiving from udp socket: {e}");
                continue;
            }
            Err(e) => return Err(e).std_context("error receiving from udp socket"),
        };

        if learn {
            let mut peer = peer.lock().unwrap();
            if *peer != Some(src) {
                tracing::debug!("learned new local peer {src}");
                *peer = Some(src);
            }
        }

        // The limit can shrink over time as the path mtu estimate changes, so
        // check it for every packet.
        let max = conn.max_datagram_size().unwrap_or(0);
        if len > max {
            counters.record_dropped(len, max);
            continue;
        }

        match conn.send_datagram(Bytes::copy_from_slice(&buf[..len])) {
            Ok(()) => {
                counters.sent.fetch_add(1, Ordering::Relaxed);
                counters.sent_bytes.fetch_add(len as u64, Ordering::Relaxed);
                tracing::trace!("udp {src} -> quic datagram ({len} bytes)");
            }
            // The path mtu estimate shrank between the check above and the
            // send. Behave like UDP and drop.
            Err(SendDatagramError::TooLarge) => counters.record_dropped(len, max),
            Err(e @ (SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled)) => {
                return Err(e).std_context("cannot send quic datagrams");
            }
            Err(SendDatagramError::ConnectionLost(e)) => {
                return Err(e).std_context("connection lost while sending datagram");
            }
        }
    }
}

/// Forward datagrams from the connection to the local UDP socket.
async fn quic_to_udp(
    conn: &Connection,
    sock: &UdpSocket,
    peer: &Mutex<Option<SocketAddr>>,
    counters: &Counters,
) -> Result<()> {
    loop {
        let data = conn
            .read_datagram()
            .await
            .std_context("error reading datagram")?;

        // On the connect side there may not be anybody to deliver to yet.
        let Some(dst) = *peer.lock().unwrap() else {
            counters.no_peer.fetch_add(1, Ordering::Relaxed);
            tracing::trace!("dropped datagram ({} bytes), no local peer yet", data.len());
            continue;
        };

        match sock.send_to(&data, dst).await {
            Ok(_) => {
                counters.recv.fetch_add(1, Ordering::Relaxed);
                counters
                    .recv_bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                tracing::trace!("quic datagram ({} bytes) -> udp {dst}", data.len());
            }
            // The local application is not listening (anymore). Normal for UDP.
            Err(e) if is_transient(&e) => {
                counters.local_failed.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("could not deliver datagram to {dst}: {e}");
            }
            Err(e) => {
                counters.local_failed.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("error sending udp packet to {dst}: {e}");
            }
        }
    }
}
