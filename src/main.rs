//! Command line arguments.
mod udp;

use std::{
    io,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use clap::{Parser, Subcommand};
use dumbpipe::{webrtc, EndpointTicket};
use iroh::{
    endpoint::{presets, Accepting},
    Endpoint, EndpointAddr, SecretKey,
};
use n0_error::{bail_any, ensure_any, AnyError, Result, StdResultExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UdpSocket,
    select,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_subscriber::EnvFilter;

const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// The highest verbosity level supported by `--verbose` / `--verbose-max`.
const MAX_VERBOSE: u8 = 3;

/// The verbosity used by subcommands that have no common args.
const DEFAULT_VERBOSITY: u8 = 0;

/// The size of the buffer used when copying between local io and noq streams.
const COPY_BUF: usize = 8 * 1024;

/// Create a dumb pipe between two machines, using an iroh endpoint.
///
/// One side listens, the other side connects. Both sides are identified by a
/// 32 byte endpoint id.
///
/// Connecting to a endpoint id is independent of its IP address. Dumbpipe will try
/// to establish a direct connection even through NATs and firewalls. If that
/// fails, it will fall back to using a relay server.
///
/// For all subcommands, you can specify a secret key using the IROH_SECRET
/// environment variable. If you don't, a random one will be generated.
///
/// You can also specify a port for the endpoint. If you don't, a random one
/// will be chosen.
#[derive(Parser, Debug)]
pub struct Args {
    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Generate a short endpoint ticket. This ticket can be used to later connect to a
    /// listener that is using the same secret key again.
    ///
    /// This command only really makes sense when you are providing dumbpipe with a
    /// secret key.
    GenerateTicket,

    /// Listen on an endpoint and forward stdin/stdout to the first incoming
    /// bidi stream.
    ///
    /// Will print a endpoint ticket on stderr that can be used to connect.
    Listen(ListenArgs),

    /// Listen on an endpoint and forward incoming connections to the specified
    /// host and port. Every incoming bidi stream is forwarded to a new connection.
    ///
    /// Will print a endpoint ticket on stderr that can be used to connect.
    ///
    /// As far as the endpoint is concerned, this is listening. But it is
    /// connecting to a TCP socket for which you have to specify the host and port.
    ListenTcp(ListenTcpArgs),

    /// Connect to an endpoint, open a bidi stream, and forward stdin/stdout.
    ///
    /// A endpoint ticket is required to connect.
    Connect(ConnectArgs),

    /// Connect to an endpoint, open a bidi stream, and forward stdin/stdout
    /// to it.
    ///
    /// A endpoint ticket is required to connect.
    ///
    /// As far as the endpoint is concerned, this is connecting. But it is
    /// listening on a TCP socket for which you have to specify the interface and port.
    ConnectTcp(ConnectTcpArgs),

    /// Listen on an endpoint and forward incoming QUIC datagrams to the
    /// specified UDP host and port, and replies back over the same connection.
    ///
    /// Will print a endpoint ticket on stderr that can be used to connect.
    ///
    /// As far as the endpoint is concerned, this is listening. But it is
    /// sending and receiving UDP datagrams for which you have to specify the
    /// host and port.
    ///
    /// Datagrams are unreliable and unordered, but preserve message
    /// boundaries and are not subject to head-of-line blocking. This makes
    /// this subcommand pair suitable for protocols that do their own
    /// retransmission and timing, such as SRT.
    ListenUdp(ListenUdpArgs),

    /// Listen on a local UDP address and forward all received datagrams to an
    /// endpoint, and replies back to the address that sent them.
    ///
    /// A endpoint ticket is required to connect.
    ///
    /// As far as the endpoint is concerned, this is connecting. But it is
    /// listening on a UDP socket for which you have to specify the address.
    ConnectUdp(ConnectUdpArgs),

    /// Listen for a WHIP stream from OBS and forward the media to viewers over
    /// an iroh connection.
    ///
    /// Will print a endpoint ticket on stderr that a viewer can connect with.
    ///
    /// As far as the endpoint is concerned, this is listening. But it is also
    /// serving HTTP, which is where OBS posts its SDP offer. Dumbpipe terminates
    /// WebRTC (ICE, DTLS, SRTP) here, because neither mpv nor VLC can do it,
    /// and forwards the media as plain RTP to the viewers.
    ///
    /// The media is forwarded unbuffered, one QUIC datagram per RTP packet, and
    /// a viewer that joins in the middle of a GOP starts at the next keyframe.
    ListenWhip(ListenWhipArgs),

    /// Connect to a WHIP host and play the stream in a local media player.
    ///
    /// A endpoint ticket is required to connect.
    ///
    /// As far as the endpoint is concerned, this is connecting. It writes the
    /// received RTP to local UDP ports and the stream description to an SDP
    /// file, and launches a player on that file.
    ConnectWhip(ConnectWhipArgs),

    #[cfg(unix)]
    /// Listen on an endpoint and forward incoming connections to the specified
    /// Unix socket path. Every incoming bidi stream is forwarded to a new connection.
    ///
    /// Will print a endpoint ticket on stderr that can be used to connect.
    ///
    /// As far as the endpoint is concerned, this is listening. But it is
    /// connecting to a Unix socket for which you have to specify the path.
    ListenUnix(ListenUnixArgs),

    #[cfg(unix)]
    /// Connect to an endpoint, open a bidi stream, and forward connections
    /// from the specified Unix socket path.
    ///
    /// A endpoint ticket is required to connect.
    ///
    /// As far as the endpoint is concerned, this is connecting. But it is
    /// listening on a Unix socket for which you have to specify the path.
    ConnectUnix(ConnectUnixArgs),
}

impl Commands {
    /// The common args of a subcommand, if it has any.
    ///
    /// Used to configure logging before the subcommand itself runs.
    fn common(&self) -> Option<&CommonArgs> {
        match self {
            Commands::GenerateTicket => None,
            Commands::Listen(args) => Some(&args.common),
            Commands::ListenTcp(args) => Some(&args.common),
            Commands::Connect(args) => Some(&args.common),
            Commands::ConnectTcp(args) => Some(&args.common),
            Commands::ListenUdp(args) => Some(&args.common),
            Commands::ConnectUdp(args) => Some(&args.common),
            Commands::ListenWhip(args) => Some(&args.common),
            Commands::ConnectWhip(args) => Some(&args.common),

            #[cfg(unix)]
            Commands::ListenUnix(args) => Some(&args.common),

            #[cfg(unix)]
            Commands::ConnectUnix(args) => Some(&args.common),
        }
    }
}

#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// The IPv4 address that the endpoint will listen on.
    ///
    /// If None, defaults to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long, default_value = None)]
    pub ipv4_addr: Option<SocketAddrV4>,

    /// The IPv6 address that the endpoint will listen on.
    ///
    /// If None, defaults to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long, default_value = None)]
    pub ipv6_addr: Option<SocketAddrV6>,

    /// A custom ALPN to use for the endpoint.
    ///
    /// This is an expert feature that allows dumbpipe to be used to interact
    /// with existing iroh protocols.
    ///
    /// When using this option, the connect side must also specify the same ALPN.
    /// The listen side will not expect a handshake, and the connect side will
    /// not send one.
    ///
    /// Alpns are byte strings. To specify an utf8 string, prefix it with `utf8:`.
    /// Otherwise, it will be parsed as a hex string.
    #[clap(long)]
    pub custom_alpn: Option<String>,

    /// The verbosity level. Repeat to increase verbosity.
    ///
    /// Each level includes everything from the levels below it.
    ///
    /// 0 (default): errors only.
    ///
    /// 1 (`-v`): info. Connection lifecycle: endpoint creation, dialing and
    /// accepting, handshakes, local socket binds, and byte/packet totals when a
    /// tunnel closes.
    ///
    /// 2 (`-vv`): debug. Everything above, plus every stream and datagram event,
    /// the negotiated ALPN and max datagram size, non-fatal errors, and a stats
    /// line every few seconds while a tunnel is running.
    ///
    /// 3 (`-vvv`): trace. Everything above, plus one line per copied chunk and
    /// per datagram with sizes and addresses, and iroh's own internal logging.
    ///
    /// Capped by `--verbose-max`. Overridden by `RUST_LOG`, which wins if set.
    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// The highest verbosity level that `--verbose` can reach.
    ///
    /// The effective verbosity is `min(verbose, verbose_max)`, so
    /// `-v -v -v --verbose-max 1` logs at info level only. This is useful when
    /// verbosity is set globally (e.g. in a wrapper script or `alias`) but the
    /// full detail would be too noisy or too slow.
    ///
    /// Defaults to 3 (trace), the highest supported level. Values above that are
    /// clamped. Ignored when `RUST_LOG` is set, since `RUST_LOG` overrides the
    /// verbosity entirely.
    #[clap(long, default_value_t = MAX_VERBOSE)]
    pub verbose_max: u8,
}

impl CommonArgs {
    fn alpn(&self) -> Result<Vec<u8>> {
        Ok(match &self.custom_alpn {
            Some(alpn) => parse_alpn(alpn)?,
            None => dumbpipe::ALPN.to_vec(),
        })
    }

    fn is_custom_alpn(&self) -> bool {
        self.custom_alpn.is_some()
    }

    /// The effective verbosity: the `-v` count, clamped to `--verbose-max`.
    ///
    /// Always in the range `0..=MAX_VERBOSE`.
    fn verbosity(&self) -> u8 {
        self.verbose.min(self.verbose_max).min(MAX_VERBOSE)
    }
}

/// Set up logging based on the verbosity flags.
///
/// `RUST_LOG` takes precedence if it is set, otherwise the default filter is
/// derived from the clamped `-v` count.
///
/// Takes the common args as an `Option` because not every subcommand has any
/// (e.g. `generate-ticket`).
fn init_logging(common: Option<&CommonArgs>) {
    let verbosity = common.map(|c| c.verbosity()).unwrap_or(DEFAULT_VERBOSITY);
    let filter = EnvFilter::builder()
        .with_default_directive(log_level(verbosity).into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Some(common) = common {
        // tell the user what they are getting, and why it might be less than
        // they asked for, but only when they asked for something
        if common.verbose > 0 {
            eprintln!("logging at {} level", log_level(verbosity));
            if verbosity < common.verbose {
                eprintln!(
                    "note: verbosity clamped from {} to {} by --verbose-max {}",
                    common.verbose, verbosity, common.verbose_max
                );
            }
        }
    }
}

/// The log level for a verbosity level.
fn log_level(verbosity: u8) -> Level {
    match verbosity {
        0 => Level::ERROR,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

/// Whether per-tunnel periodic stats should be logged while running.
fn stats_enabled(verbosity: u8) -> bool {
    verbosity >= 2
}

fn parse_alpn(alpn: &str) -> Result<Vec<u8>> {
    Ok(if let Some(text) = alpn.strip_prefix("utf8:") {
        text.as_bytes().to_vec()
    } else {
        hex::decode(alpn).anyerr()?
    })
}

/// The ALPN to use for a udp tunnel.
///
/// Unless a custom ALPN is given, the dedicated udp ALPN is used instead of
/// the stream ALPN, so that udp and stream tunnels cannot be confused.
fn udp_alpn(common: &CommonArgs) -> Result<Vec<u8>> {
    Ok(match &common.custom_alpn {
        Some(alpn) => parse_alpn(alpn)?,
        None => udp::ALPN.to_vec(),
    })
}

/// The ALPN to use for a webrtc tunnel.
///
/// Unless a custom ALPN is given, the dedicated webrtc ALPN is used, so that a
/// webrtc viewer can never be mistaken for a stream or udp connector.
fn webrtc_alpn(common: &CommonArgs) -> Result<Vec<u8>> {
    Ok(match &common.custom_alpn {
        Some(alpn) => parse_alpn(alpn)?,
        None => dumbpipe::WEBRTC_ALPN.to_vec(),
    })
}

#[derive(Parser, Debug)]
pub struct ListenArgs {
    /// Immediately close our sending side, indicating that we will not transmit any data
    #[clap(long)]
    pub recv_only: bool,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ListenTcpArgs {
    #[clap(long)]
    pub host: String,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ConnectTcpArgs {
    /// The addresses to listen on for incoming tcp connections.
    ///
    /// To listen on all network interfaces, use 0.0.0.0:12345
    #[clap(long)]
    pub addr: String,

    /// The endpoint to connect to
    pub ticket: EndpointTicket,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ListenUdpArgs {
    /// The UDP host and port that incoming datagrams are forwarded to.
    ///
    /// Replies from that host and port are forwarded back over the tunnel.
    ///
    /// To forward to a local SRT listener, use 127.0.0.1:9000
    #[clap(long)]
    pub host: String,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ConnectUdpArgs {
    /// The UDP address to listen on for incoming datagrams.
    ///
    /// Datagrams received here are forwarded to the endpoint in the ticket.
    ///
    /// To listen on all network interfaces, use 0.0.0.0:9000
    #[clap(long)]
    pub addr: String,

    /// The endpoint to connect to
    pub ticket: EndpointTicket,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ListenWhipArgs {
    /// The HTTP address to serve WHIP on.
    ///
    /// This is the URL you give OBS in its WHIP output settings, e.g.
    /// http://127.0.0.1:8080/whip
    ///
    /// To accept WHIP from other machines on the local network, use
    /// 0.0.0.0:8080 and set --ice-addr to the address OBS can reach.
    #[clap(long, default_value = webrtc::DEFAULT_WHIP_ADDR)]
    pub listen: String,

    /// The bearer token OBS has to present.
    ///
    /// If set, requests without `Authorization: Bearer <token>` are rejected
    /// with 401. OBS has a field for exactly this token.
    #[clap(long)]
    pub bearer_token: Option<String>,

    /// The address to bind the ICE socket on.
    ///
    /// By default a loopback `--listen` address gets a loopback ICE socket and
    /// everything else the address of the interface that routes to the internet.
    /// Set this explicitly on a machine with several interfaces or a VPN.
    #[clap(long)]
    pub ice_addr: Option<SocketAddr>,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ConnectWhipArgs {
    /// The endpoint to connect to
    pub ticket: EndpointTicket,

    /// The local address to play the stream out on.
    ///
    /// RTP is delivered to the even port of this address, RTCP to the port
    /// above it, and audio to the next pair, so 5004 needs 5004 to 5007 free.
    #[clap(long, default_value = webrtc::DEFAULT_PLAY_ADDR)]
    pub addr: String,

    /// Where to write the SDP file that describes the stream to the player.
    ///
    /// The value is optional: `--sdp` on its own writes the default file
    /// (`dumbpipe-<port>.sdp`) into the current folder, so you can open it by
    /// hand. Give a path with an equals sign, `--sdp=/tmp/mine.sdp`, to choose
    /// the name, or a directory to put the default file inside it.
    #[clap(long, num_args(0..=1), require_equals(true))]
    pub sdp: Option<Option<PathBuf>>,

    /// The media player to launch on the SDP file.
    ///
    /// mpv is the default and was verified to work, together with ffplay. VLC
    /// is offered as an alternative but could not be made to accept a plain RTP
    /// SDP on the 3.0.x build this was tested with, so use it with care.
    #[clap(long, value_enum, default_value_t = webrtc::Player::Mpv)]
    pub player: webrtc::Player,

    /// The path to the player binary, for a player that is not on `PATH`.
    ///
    /// The flags are still those of `--player`, which defaults to `mpv`. So
    /// `--player-path /opt/mpv` runs `/opt/mpv` with the mpv flags, and
    /// `--player ffplay --player-path /opt/ffplay` runs `/opt/ffplay` with the
    /// ffplay flags.
    #[clap(long)]
    pub player_path: Option<PathBuf>,

    /// Do not launch a player, only write the SDP file.
    ///
    /// The command line to play the stream yourself is printed instead.
    #[clap(long)]
    pub no_launch: bool,

    /// How much jitter buffer to give the player, in milliseconds.
    ///
    /// The default is the lowest latency, which is right on a local network.
    /// Across a country or an ocean the path jitters, reorders and drops
    /// packets, and a receiver with no buffer drops them too: you get decode
    /// errors and audio that drifts out of sync. Set this to 200-500 for a
    /// long-distance link and the player will absorb the jitter instead.
    #[clap(long)]
    pub buffer: Option<u64>,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct ConnectArgs {
    /// The endpoint to connect to
    pub ticket: EndpointTicket,

    /// Immediately close our sending side, indicating that we will not transmit any data
    #[clap(long)]
    pub recv_only: bool,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[cfg(unix)]
#[derive(Parser, Debug)]
pub struct ListenUnixArgs {
    /// Path to the Unix socket to connect to
    #[clap(long)]
    pub socket_path: PathBuf,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[cfg(unix)]
#[derive(Parser, Debug)]
pub struct ConnectUnixArgs {
    /// Path to the Unix socket to listen on
    #[clap(long)]
    pub socket_path: PathBuf,

    /// The endpoint to connect to
    pub ticket: EndpointTicket,

    #[clap(flatten)]
    pub common: CommonArgs,
}

/// Copy from a reader to a writer, logging every chunk and the total.
///
/// This is what makes `-vvv` noisy: one trace line per chunk. The totals are
/// logged at debug level, so they are visible from `-vv` upwards.
///
/// Returns the number of bytes copied.
async fn copy_verbose(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (impl AsyncWrite + Unpin),
    direction: &'static str,
) -> io::Result<u64> {
    let mut buf = vec![0u8; COPY_BUF];
    let mut total: u64 = 0;
    let mut chunks: u64 = 0;
    loop {
        let len = from.read(&mut buf).await?;
        if len == 0 {
            break;
        }
        to.write_all(&buf[..len]).await?;
        total += len as u64;
        chunks += 1;
        tracing::trace!(
            direction,
            chunk = chunks,
            bytes = len,
            total,
            "copied chunk"
        );
    }
    tracing::debug!(direction, chunks, total, "copy finished");
    Ok(total)
}

/// Copy from a reader to a noq stream.
///
/// Will send a reset to the other side if the operation is cancelled, and fail
/// with an error.
///
/// Returns the number of bytes copied in case of success.
async fn copy_to_noq(
    mut from: impl AsyncRead + Unpin,
    mut send: noq::SendStream,
    token: CancellationToken,
) -> io::Result<u64> {
    tracing::debug!("copying local io to quic stream");
    tokio::select! {
        res = copy_verbose(&mut from, &mut send, "local -> quic") => {
            let size = res?;
            send.finish()?;
            Ok(size)
        }
        _ = token.cancelled() => {
            tracing::debug!("copy to quic stream cancelled, resetting stream");
            // send a reset to the other side immediately
            send.reset(0u8.into()).ok();
            Err(io::Error::other("cancelled"))
        }
    }
}

/// Copy from a noq stream to a writer.
///
/// Will send stop to the other side if the operation is cancelled, and fail
/// with an error.
///
/// Returns the number of bytes copied in case of success.
async fn copy_from_noq(
    mut recv: noq::RecvStream,
    mut to: impl AsyncWrite + Unpin,
    token: CancellationToken,
) -> io::Result<u64> {
    tracing::debug!("copying quic stream to local io");
    tokio::select! {
        res = copy_verbose(&mut recv, &mut to, "quic -> local") => {
            Ok(res?)
        },
        _ = token.cancelled() => {
            tracing::debug!("copy from quic stream cancelled, stopping stream");
            recv.stop(0u8.into()).ok();
            Err(io::Error::other("cancelled"))
        }
    }
}

/// Read and verify the handshake from a noq stream.
async fn read_handshake(recv: &mut noq::RecvStream) -> Result<()> {
    let mut buf = [0u8; dumbpipe::HANDSHAKE.len()];
    recv.read_exact(&mut buf).await.anyerr()?;
    ensure_any!(buf == dumbpipe::HANDSHAKE, "invalid handshake");
    tracing::debug!("handshake verified");
    Ok(())
}

/// Write the handshake to a noq stream.
async fn write_handshake(send: &mut noq::SendStream) -> Result<()> {
    send.write_all(&dumbpipe::HANDSHAKE).await.anyerr()?;
    tracing::debug!("handshake sent");
    Ok(())
}

/// Get the secret key or generate a new one.
///
/// Print the secret key to stderr if it was generated, so the user can save it.
fn get_or_create_secret() -> Result<SecretKey> {
    match std::env::var("IROH_SECRET") {
        Ok(secret) => SecretKey::from_str(&secret).std_context("invalid secret"),
        Err(_) => {
            let key = SecretKey::generate();
            eprintln!(
                "using secret key {}",
                data_encoding::HEXLOWER.encode(&key.to_bytes())
            );
            Ok(key)
        }
    }
}

/// Create a new iroh endpoint.
async fn create_endpoint(
    secret_key: SecretKey,
    common: &CommonArgs,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    for alpn in &alpns {
        tracing::debug!(
            "endpoint alpn: {} (hex {})",
            String::from_utf8_lossy(alpn),
            hex::encode(alpn)
        );
    }
    if alpns.is_empty() {
        tracing::debug!("endpoint has no alpns (connect-only)");
    }
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(alpns);
    if let Some(addr) = common.ipv4_addr {
        tracing::debug!("binding ipv4 addr {addr}");
        builder = builder.bind_addr(addr)?;
    }
    if let Some(addr) = common.ipv6_addr {
        tracing::debug!("binding ipv6 addr {addr}");
        builder = builder.bind_addr(addr)?;
    }
    let endpoint = builder.bind().await.anyerr()?;
    tracing::debug!("endpoint bound: {:?}", endpoint.addr());
    Ok(endpoint)
}

fn cancel_token<T>(token: CancellationToken) -> impl Fn(T) -> T {
    move |x| {
        token.cancel();
        x
    }
}

/// Bidirectionally forward data from a noq stream and an arbitrary tokio
/// reader/writer pair, aborting both sides when either one forwarder is done,
/// or when control-c is pressed.
async fn forward_bidi(
    from1: impl AsyncRead + Send + Sync + Unpin + 'static,
    to1: impl AsyncWrite + Send + Sync + Unpin + 'static,
    from2: noq::RecvStream,
    to2: noq::SendStream,
) -> Result<()> {
    let token1 = CancellationToken::new();
    let token2 = token1.clone();
    let token3 = token1.clone();
    let forward_from_stdin = tokio::spawn(async move {
        copy_to_noq(from1, to2, token1.clone())
            .await
            .map_err(cancel_token(token1))
    });
    let forward_to_stdout = tokio::spawn(async move {
        copy_from_noq(from2, to1, token2.clone())
            .await
            .map_err(cancel_token(token2))
    });
    let _control_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await?;
        token3.cancel();
        io::Result::Ok(())
    });
    let to_local = forward_to_stdout.await.anyerr()?.anyerr()?;
    let to_remote = forward_from_stdin.await.anyerr()?.anyerr()?;
    tracing::info!(
        "stream closed: {to_local} bytes quic -> local, {to_remote} bytes local -> quic"
    );
    Ok(())
}

async fn listen_stdio(args: ListenArgs) -> Result<()> {
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![args.common.alpn()?]).await?;
    // wait for the endpoint to figure out its home relay and addresses before making a ticket
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }
    let addr = endpoint.addr();
    let short = create_short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);

    // print the ticket on stderr so it doesn't interfere with the data itself
    //
    // note that the tests rely on the ticket being the last thing printed
    eprintln!("Listening. To connect, use:\ndumbpipe connect {ticket}");
    if args.common.verbose > 0 {
        eprintln!("or:\ndumbpipe connect {short}");
    }
    tracing::info!("waiting for connections");

    loop {
        let Some(connecting) = endpoint.accept().await else {
            break;
        };
        let connection = match connecting.await {
            Ok(connection) => connection,
            Err(cause) => {
                tracing::warn!("error accepting connection: {}", cause);
                // if accept fails, we want to continue accepting connections
                continue;
            }
        };
        let remote_endpoint_id = &connection.remote_id();
        tracing::info!("got connection from {}", remote_endpoint_id);
        let (s, mut r) = match connection.accept_bi().await {
            Ok(x) => x,
            Err(cause) => {
                tracing::warn!("error accepting stream: {}", cause);
                // if accept_bi fails, we want to continue accepting connections
                continue;
            }
        };
        tracing::info!("accepted bidi stream from {}", remote_endpoint_id);
        if !args.common.is_custom_alpn() {
            // read the handshake and verify it
            read_handshake(&mut r).await?;
        }
        if args.recv_only {
            tracing::info!(
                "forwarding stdout to {} (ignoring stdin)",
                remote_endpoint_id
            );
            forward_bidi(tokio::io::empty(), tokio::io::stdout(), r, s).await?;
        } else {
            tracing::info!("forwarding stdin/stdout to {}", remote_endpoint_id);
            forward_bidi(tokio::io::stdin(), tokio::io::stdout(), r, s).await?;
        }
        // stop accepting connections after the first successful one
        break;
    }
    endpoint.close().await;
    Ok(())
}

async fn connect_stdio(args: ConnectArgs) -> Result<()> {
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![]).await?;
    let addr = args.ticket.endpoint_addr();
    let remote_endpoint_id = addr.id;
    tracing::info!("connecting to {}", remote_endpoint_id);
    // connect to the remote, try only once
    let connection = endpoint
        .connect(addr.clone(), &args.common.alpn()?)
        .await
        .anyerr()?;
    tracing::info!("connected to {}", remote_endpoint_id);
    // open a bidi stream, try only once
    let (mut s, r) = connection.open_bi().await.anyerr()?;
    tracing::info!("opened bidi stream to {}", remote_endpoint_id);
    // send the handshake unless we are using a custom alpn
    // when using a custom alpn, everything is up to the user
    if !args.common.is_custom_alpn() {
        // the connecting side must write first. we don't know if there will be something
        // on stdin, so just write a handshake.
        write_handshake(&mut s).await?;
    }
    if args.recv_only {
        tracing::info!(
            "forwarding stdout to {} (ignoring stdin)",
            remote_endpoint_id
        );
        forward_bidi(tokio::io::empty(), tokio::io::stdout(), r, s).await?;
    } else {
        tracing::info!("forwarding stdin/stdout to {}", remote_endpoint_id);
        forward_bidi(tokio::io::stdin(), tokio::io::stdout(), r, s).await?;
    }
    tokio::io::stdout().flush().await.anyerr()?;
    endpoint.close().await;
    Ok(())
}

/// Listen on a tcp port and forward incoming connections to an endpoint.
async fn connect_tcp(args: ConnectTcpArgs) -> Result<()> {
    let addrs = args
        .addr
        .to_socket_addrs()
        .std_context(format!("invalid host string {}", args.addr))?;
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![])
        .await
        .std_context("unable to bind endpoint")?;
    tracing::info!("tcp listening on {:?}", addrs);

    // Wait for our own endpoint to be ready before trying to connect.
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }

    let tcp_listener = match tokio::net::TcpListener::bind(addrs.as_slice()).await {
        Ok(tcp_listener) => tcp_listener,
        Err(cause) => {
            tracing::error!("error binding tcp socket to {:?}: {}", addrs, cause);
            return Ok(());
        }
    };
    async fn handle_tcp_accept(
        next: io::Result<(tokio::net::TcpStream, SocketAddr)>,
        addr: EndpointAddr,
        endpoint: Endpoint,
        handshake: bool,
        alpn: &[u8],
    ) -> Result<()> {
        let (tcp_stream, tcp_addr) = next.std_context("error accepting tcp connection")?;
        let (tcp_recv, tcp_send) = tcp_stream.into_split();
        tracing::info!("got tcp connection from {}", tcp_addr);
        let remote_endpoint_id = addr.id;
        tracing::debug!(
            "dialing {remote_endpoint_id} with alpn {} (hex {})",
            String::from_utf8_lossy(alpn),
            hex::encode(alpn)
        );
        let connection = endpoint
            .connect(addr, alpn)
            .await
            .std_context(format!("error connecting to {remote_endpoint_id}"))?;
        tracing::info!("connected to {}", remote_endpoint_id);
        let (mut endpoint_send, endpoint_recv) = connection
            .open_bi()
            .await
            .std_context(format!("error opening bidi stream to {remote_endpoint_id}"))?;
        // send the handshake unless we are using a custom alpn
        // when using a custom alpn, everything is up to the user
        if handshake {
            // the connecting side must write first. we don't know if there will be something
            // on stdin, so just write a handshake.
            write_handshake(&mut endpoint_send).await?;
        }
        forward_bidi(tcp_recv, tcp_send, endpoint_recv, endpoint_send).await?;
        Ok::<_, AnyError>(())
    }
    let addr = args.ticket.endpoint_addr();
    loop {
        // also wait for ctrl-c here so we can use it before accepting a connection
        let next = tokio::select! {
            stream = tcp_listener.accept() => stream,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("got ctrl-c, exiting");
                break;
            }
        };
        let endpoint = endpoint.clone();
        let addr = addr.clone();
        let handshake = !args.common.is_custom_alpn();
        let alpn = args.common.alpn()?;
        tokio::spawn(async move {
            if let Err(cause) = handle_tcp_accept(next, addr, endpoint, handshake, &alpn).await {
                // log error at warn level
                //
                // we should know about it, but it's not fatal
                tracing::warn!("error handling connection: {}", cause);
            }
        });
    }
    endpoint.close().await;
    Ok(())
}

/// Listen on an endpoint and forward incoming connections to a tcp socket.
async fn listen_tcp(args: ListenTcpArgs) -> Result<()> {
    let addrs = match args.host.to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(e) => bail_any!("invalid host string {}: {}", args.host, e),
    };
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![args.common.alpn()?]).await?;
    // wait for the endpoint to figure out its address before making a ticket
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }
    let addr = endpoint.addr();
    let short = create_short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);

    // print the ticket on stderr so it doesn't interfere with the data itself
    //
    // note that the tests rely on the ticket being the last thing printed
    eprintln!("Forwarding incoming requests to '{}'.", args.host);
    eprintln!("To connect, use e.g.:");
    eprintln!("dumbpipe connect-tcp {ticket}");
    if args.common.verbose > 0 {
        eprintln!("or:\ndumbpipe connect-tcp {short}");
    }
    tracing::info!("waiting for connections");
    tracing::info!("endpoint id is {}", ticket.endpoint_addr().id);
    tracing::info!(
        "relay url is {:?}",
        ticket
            .endpoint_addr()
            .relay_urls()
            .next()
            .map_or("None".to_string(), |url| url.to_string())
    );

    // handle a new incoming connection on the endpoint
    async fn handle_endpoint_accept(
        accepting: Accepting,
        addrs: Vec<std::net::SocketAddr>,
        handshake: bool,
    ) -> Result<()> {
        let connection = accepting.await.std_context("error accepting connection")?;
        let remote_endpoint_id = &connection.remote_id();
        tracing::info!("got connection from {}", remote_endpoint_id);
        let (s, mut r) = connection
            .accept_bi()
            .await
            .std_context("error accepting stream")?;
        tracing::info!("accepted bidi stream from {}", remote_endpoint_id);
        if handshake {
            // read the handshake and verify it
            read_handshake(&mut r).await?;
        }
        let connection = tokio::net::TcpStream::connect(addrs.as_slice())
            .await
            .std_context(format!("error connecting to {addrs:?}"))?;
        let (read, write) = connection.into_split();
        forward_bidi(read, write, r, s).await?;
        Ok(())
    }

    loop {
        let incoming = select! {
            incoming = endpoint.accept() => incoming,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("got ctrl-c, exiting");
                break;
            }
        };
        let Some(incoming) = incoming else {
            break;
        };
        let accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(err) => {
                tracing::warn!("error accepting connection: {err}");
                // if accept fails, we want to continue accepting connections
                continue;
            }
        };
        let addrs = addrs.clone();
        let handshake = !args.common.is_custom_alpn();
        tokio::spawn(async move {
            if let Err(cause) = handle_endpoint_accept(accepting, addrs, handshake).await {
                // log error at warn level
                //
                // we should know about it, but it's not fatal
                tracing::warn!("error handling connection: {}", cause);
            }
        });
    }
    endpoint.close().await;
    Ok(())
}

/// Listen on an endpoint and forward incoming datagrams to a udp address.
///
/// Each incoming connection gets its own ephemeral udp socket, so that
/// multiple connectors become multiple independent flows towards the target.
async fn listen_udp(args: ListenUdpArgs) -> Result<()> {
    let target = udp::resolve(&args.host)?;
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![udp_alpn(&args.common)?]).await?;
    // wait for the endpoint to figure out its address before making a ticket
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }
    let addr = endpoint.addr();
    let short = create_short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);

    // print the ticket on stderr so it doesn't interfere with the data itself
    //
    // note that the tests rely on the ticket being the last thing printed
    eprintln!("Forwarding incoming datagrams to udp://{target}.");
    eprintln!("To connect, use e.g.:");
    eprintln!("dumbpipe connect-udp --addr 127.0.0.1:9000 {ticket}");
    if args.common.verbose > 0 {
        eprintln!("or:\ndumbpipe connect-udp --addr 127.0.0.1:9000 {short}");
    }
    tracing::info!("endpoint id is {}", ticket.endpoint_addr().id);
    tracing::info!(
        "relay url is {:?}",
        ticket
            .endpoint_addr()
            .relay_urls()
            .next()
            .map_or("None".to_string(), |url| url.to_string())
    );
    let alpn = udp_alpn(&args.common)?;
    tracing::debug!(
        "alpn is {} (hex {})",
        String::from_utf8_lossy(&alpn),
        hex::encode(&alpn)
    );
    let stats = stats_enabled(args.common.verbosity());
    tracing::info!("waiting for connections");

    // handle a new incoming connection on the endpoint
    async fn handle_endpoint_accept(
        accepting: Accepting,
        target: SocketAddr,
        handshake: bool,
        stats: bool,
    ) -> Result<()> {
        let connection = accepting.await.std_context("error accepting connection")?;
        let remote_endpoint_id = &connection.remote_id();
        tracing::info!("got connection from {}", remote_endpoint_id);
        // The udp flow belongs to this connection. The bidi stream opened by the
        // connecting side is only used for the handshake, but is kept open for
        // the lifetime of the tunnel as a liveness signal, while the datagrams
        // ride along on the same connection.
        let (_send, mut recv) = connection
            .accept_bi()
            .await
            .std_context("error accepting stream")?;
        tracing::info!("accepted bidi stream from {}", remote_endpoint_id);
        if handshake {
            // read the handshake and verify it
            read_handshake(&mut recv).await?;
            tracing::debug!("handshake verified for {remote_endpoint_id}");
        }
        let socket = Arc::new(udp::bind_for(target).await?);
        tracing::info!("forwarding datagrams between {remote_endpoint_id} and udp://{target}");
        udp::bridge(connection, socket, Some(target), stats).await?;
        Ok(())
    }

    loop {
        let incoming = select! {
            incoming = endpoint.accept() => incoming,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("got ctrl-c, exiting");
                break;
            }
        };
        let Some(incoming) = incoming else {
            break;
        };
        let accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(err) => {
                tracing::warn!("error accepting connection: {err}");
                // if accept fails, we want to continue accepting connections
                continue;
            }
        };
        let handshake = !args.common.is_custom_alpn();
        tokio::spawn(async move {
            if let Err(cause) = handle_endpoint_accept(accepting, target, handshake, stats).await {
                // log error at warn level
                //
                // we should know about it, but it's not fatal
                tracing::warn!("error handling connection: {}", cause);
            }
        });
    }
    endpoint.close().await;
    Ok(())
}

/// Listen on a udp address and forward incoming datagrams to an endpoint.
///
/// Datagrams coming back from the endpoint are sent to whichever local address
/// most recently sent a datagram, which is what a single client, e.g. an SRT
/// caller, needs.
async fn connect_udp(args: ConnectUdpArgs) -> Result<()> {
    let addr = udp::resolve(&args.addr)?;
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![])
        .await
        .std_context("unable to bind endpoint")?;

    // Wait for our own endpoint to be ready before trying to connect.
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }

    let socket = Arc::new(
        UdpSocket::bind(addr)
            .await
            .std_context(format!("error binding udp socket to {addr}"))?,
    );
    tracing::info!(
        "udp listening on udp://{}",
        socket
            .local_addr()
            .std_context("error getting local address")?
    );

    let remote_addr = args.ticket.endpoint_addr();
    let remote_endpoint_id = remote_addr.id;
    let alpn = udp_alpn(&args.common)?;
    tracing::debug!(
        "dialing {remote_endpoint_id} with alpn {} (hex {})",
        String::from_utf8_lossy(&alpn),
        hex::encode(&alpn)
    );
    tracing::info!("connecting to {}", remote_endpoint_id);
    let connection = endpoint
        .connect(remote_addr.clone(), &alpn)
        .await
        .std_context(format!("error connecting to {remote_endpoint_id}"))?;
    tracing::info!("connected to {}", remote_endpoint_id);

    // open a bidi stream, try only once
    let (mut send, recv) = connection
        .open_bi()
        .await
        .std_context(format!("error opening bidi stream to {remote_endpoint_id}"))?;
    // send the handshake unless we are using a custom alpn
    // when using a custom alpn, everything is up to the user
    if !args.common.is_custom_alpn() {
        // the connecting side must write first, so just write a handshake.
        write_handshake(&mut send).await?;
    }
    // keep the stream open for the lifetime of the tunnel
    let _stream = (send, recv);
    tracing::info!("tunnel established, forwarding udp to {remote_endpoint_id}");

    let stats = stats_enabled(args.common.verbosity());
    tokio::select! {
        res = udp::bridge(connection.clone(), socket, None, stats) => {
            res?;
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("got ctrl-c, exiting");
        }
    }

    tracing::info!("closing connection to {}", remote_endpoint_id);
    connection.close(0u32.into(), b"udp tunnel closed");
    endpoint.close().await;
    Ok(())
}

/// Creates a ticket that only includes the id and any relay urls
fn create_short_ticket(addr: &EndpointAddr) -> EndpointTicket {
    let mut short = EndpointAddr::new(addr.id);
    for relay_url in addr.relay_urls() {
        short = short.with_relay_url(relay_url.clone());
    }
    short.into()
}

/// Serve WHIP on a local HTTP address and forward the media to viewers.
///
/// OBS posts its SDP offer to `http://<listen>/whip`, dumbpipe terminates the
/// WebRTC session and forwards the media as plain RTP over iroh. Every viewer
/// that dials in gets the stream from the next keyframe.
async fn listen_whip(args: ListenWhipArgs) -> Result<()> {
    let listen = udp::resolve(&args.listen)?;
    let secret_key = get_or_create_secret()?;
    let alpn = webrtc_alpn(&args.common)?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![alpn.clone()]).await?;
    // wait for the endpoint to figure out its address before making a ticket
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }
    let addr = endpoint.addr();
    let short = create_short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);

    // print the ticket on stderr so it doesn't interfere with the data itself
    //
    // note that the tests rely on the ticket being the last thing printed
    eprintln!("Serving WHIP input on http://{listen}/whip.");
    eprintln!("Use that as the server URL of the WHIP output in OBS.");
    eprintln!("To watch the stream, connect with e.g.:");
    eprintln!("dumbpipe connect-whip {ticket}");
    if args.common.verbose > 0 {
        eprintln!("or:\ndumbpipe connect-whip {short}");
    }
    tracing::info!("endpoint id is {}", ticket.endpoint_addr().id);
    tracing::debug!(
        "alpn is {} (hex {})",
        String::from_utf8_lossy(&alpn),
        hex::encode(&alpn)
    );

    let cfg = webrtc::WhipConfig {
        listen,
        bearer_token: args.bearer_token.clone(),
        ice_addr: args.ice_addr,
        stats: stats_enabled(args.common.verbosity()),
    };
    webrtc::listen_whip(endpoint, cfg).await
}

/// Connect to a WHIP host and play the stream in a local media player.
async fn connect_whip(args: ConnectWhipArgs) -> Result<()> {
    let play = udp::resolve(&args.addr)?;
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![])
        .await
        .std_context("unable to bind endpoint")?;

    // Wait for our own endpoint to be ready before trying to connect.
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }

    let alpn = webrtc_alpn(&args.common)?;
    let remote_addr = args.ticket.endpoint_addr();
    tracing::debug!(
        "dialing {} with alpn {} (hex {})",
        remote_addr.id,
        String::from_utf8_lossy(&alpn),
        hex::encode(&alpn)
    );

    // `--sdp=path` wins; a bare `--sdp`, or `--player none`, drops the
    // default-named file in the current folder for the user to open.
    let (sdp, sdp_here) = match args.sdp.clone() {
        Some(Some(p)) => (Some(p), false),
        Some(None) => (None, true),
        None => (None, args.player == webrtc::Player::None),
    };

    let cfg = webrtc::ViewerConfig {
        addr: remote_addr.clone(),
        alpn,
        play,
        sdp,
        sdp_here,
        player: args.player,
        player_path: args.player_path.clone(),
        no_launch: args.no_launch,
        buffer: args.buffer.map(Duration::from_millis),
        stats: stats_enabled(args.common.verbosity()),
    };

    let result = select! {
        res = webrtc::connect_whip(endpoint.clone(), cfg) => {
            res
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("got ctrl-c, exiting");
            Ok(())
        }
    };

    tracing::info!("closing connection to {}", remote_addr.id);
    endpoint.close().await;
    result
}

#[cfg(unix)]
/// Listen on an endpoint and forward incoming connections to a Unix socket.
async fn listen_unix(args: ListenUnixArgs) -> Result<()> {
    let socket_path = args.socket_path.clone();
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![args.common.alpn()?]).await?;
    // wait for the endpoint to figure out its address before making a ticket
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }
    let addr = endpoint.addr();
    let short = create_short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);

    // print the ticket on stderr so it doesn't interfere with the data itself
    //
    // note that the tests rely on the ticket being the last thing printed
    eprintln!(
        "Forwarding incoming requests to '{}'.",
        socket_path.display()
    );
    eprintln!("To connect, use e.g.:");
    eprintln!("dumbpipe connect-unix --socket-path /path/to/client.sock {ticket}");
    eprintln!("dumbpipe connect-tcp --addr 127.0.0.1:8080 {ticket}");
    if args.common.verbose > 0 {
        eprintln!("or:\ndumbpipe connect-unix --socket-path /path/to/client.sock {short}");
        eprintln!("dumbpipe connect-tcp --addr 127.0.0.1:8080 {short}");
    }
    tracing::info!("waiting for connections");
    tracing::info!("endpoint id is {}", ticket.endpoint_addr().id);
    tracing::info!(
        "relay url is {:?}",
        ticket
            .endpoint_addr()
            .relay_urls()
            .next()
            .map_or("None".to_string(), |url| url.to_string())
    );

    // handle a new incoming connection on the endpoint
    async fn handle_endpoint_accept(
        accepting: Accepting,
        socket_path: PathBuf,
        handshake: bool,
    ) -> Result<()> {
        tracing::trace!("accepting connection");
        let connection = accepting.await.std_context("error accepting connection")?;
        let remote_endpoint_id = &connection.remote_id();
        tracing::info!("got connection from {}", remote_endpoint_id);
        let (s, mut r) = connection
            .accept_bi()
            .await
            .std_context("error accepting stream")?;
        tracing::info!("accepted bidi stream from {}", remote_endpoint_id);
        if handshake {
            // read the handshake and verify it
            tracing::trace!("reading handshake");
            read_handshake(&mut r).await?;
            tracing::trace!("handshake verified");
        }
        tracing::trace!("connecting to backend socket {:?}", socket_path);
        let connection = UnixStream::connect(&socket_path)
            .await
            .std_context(format!("error connecting to {socket_path:?}"))?;
        tracing::trace!("connected to backend socket");
        let (read, write) = connection.into_split();
        tracing::trace!("starting forward_bidi");
        forward_bidi(read, write, r, s).await?;
        tracing::trace!("forward_bidi finished");
        Ok(())
    }

    loop {
        let incoming = select! {
            incoming = endpoint.accept() => incoming,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("got ctrl-c, exiting");
                break;
            }
        };
        let Some(incoming) = incoming else {
            break;
        };
        let accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(err) => {
                tracing::warn!("error accepting connection: {err}");
                // if accept fails, we want to continue accepting connections
                continue;
            }
        };
        let socket_path = socket_path.clone();
        let handshake = !args.common.is_custom_alpn();
        tokio::spawn(async move {
            if let Err(cause) = handle_endpoint_accept(accepting, socket_path, handshake).await {
                // log error at warn level
                //
                // we should know about it, but it's not fatal
                tracing::warn!("error handling connection: {}", cause);
            }
        });
    }
    endpoint.close().await;
    Ok(())
}

#[cfg(unix)]
/// A RAII guard to clean up a Unix socket file.
struct UnixSocketGuard {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for UnixSocketGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::error!("failed to remove socket file {:?}: {}", self.path, e);
            }
        }
    }
}

#[cfg(unix)]
/// Listen on a Unix socket and forward connections to an endpoint.
async fn connect_unix(args: ConnectUnixArgs) -> Result<()> {
    let socket_path = args.socket_path.clone();
    let secret_key = get_or_create_secret()?;
    let endpoint = create_endpoint(secret_key, &args.common, vec![])
        .await
        .std_context("unable to bind endpoint")?;
    tracing::info!("unix listening on {:?}", socket_path);

    // Wait for our own endpoint to be ready before trying to connect.
    if (timeout(ONLINE_TIMEOUT, endpoint.online()).await).is_err() {
        eprintln!("Warning: Failed to connect to the home relay");
    }

    // Remove existing socket file if it exists
    if let Err(e) = tokio::fs::remove_file(&socket_path).await {
        if e.kind() != io::ErrorKind::NotFound {
            bail_any!("failed to remove existing socket file: {}", e);
        }
    }

    let addr = args.ticket.endpoint_addr();
    tracing::info!("connecting to remote endpoint: {:?}", addr);
    let connection = endpoint
        .connect(addr.clone(), &args.common.alpn()?)
        .await
        .std_context("failed to connect to remote endpoint")?;
    tracing::info!("connected to remote endpoint successfully");

    let unix_listener = UnixListener::bind(&socket_path)
        .with_std_context(|_| format!("failed to bind Unix socket at {socket_path:?}"))?;
    tracing::info!("bound local unix socket: {:?}", socket_path);

    let _guard = UnixSocketGuard {
        path: socket_path.clone(),
    };

    async fn handle_unix_accept(
        next: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
        connection: iroh::endpoint::Connection,
        handshake: bool,
    ) -> Result<()> {
        tracing::trace!("handling new local connection");
        let (unix_stream, unix_addr) = next.std_context("error accepting unix connection")?;
        let (unix_recv, unix_send) = unix_stream.into_split();
        tracing::trace!("got unix connection from {:?}", unix_addr);

        tracing::trace!("opening bidi stream");
        let (mut endpoint_send, endpoint_recv) = connection
            .open_bi()
            .await
            .std_context("error opening bidi stream")?;
        tracing::trace!("bidi stream opened");

        // send the handshake unless we are using a custom alpn
        // when using a custom alpn, everything is up to the user
        if handshake {
            tracing::trace!("sending handshake");
            // the connecting side must write first. we don't know if there will be something
            // on stdin, so just write a handshake.
            write_handshake(&mut endpoint_send).await?;
            tracing::trace!("handshake sent");
        }

        tracing::trace!("starting forward_bidi");
        forward_bidi(unix_recv, unix_send, endpoint_recv, endpoint_send).await?;
        tracing::trace!("forward_bidi finished");
        Ok(())
    }

    tracing::info!("entering accept loop");
    loop {
        // also wait for ctrl-c here so we can use it before accepting a connection
        let next = tokio::select! {
            stream = unix_listener.accept() => stream,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("got ctrl-c, exiting");
                break;
            }
        };
        tracing::trace!("accepted a local connection");
        let connection = connection.clone();
        let handshake = !args.common.is_custom_alpn();
        tokio::spawn(async move {
            tracing::trace!("spawning handler task");
            if let Err(cause) = handle_unix_accept(next, connection, handshake).await {
                // log error at warn level
                //
                // we should know about it, but it's not fatal
                tracing::warn!("error handling connection: {}", cause);
            }
            tracing::trace!("handler task finished");
        });
    }

    endpoint.close().await;
    Ok(())
}

async fn generate_ticket() -> Result<()> {
    let secret_key = get_or_create_secret()?;
    let public_key = secret_key.public();
    let addr = EndpointAddr::new(public_key);
    let ticket = EndpointTicket::new(addr);
    println!("{}", ticket);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.command.common());
    let res = match args.command {
        Commands::GenerateTicket => generate_ticket().await,
        Commands::Listen(args) => listen_stdio(args).await,
        Commands::ListenTcp(args) => listen_tcp(args).await,
        Commands::Connect(args) => connect_stdio(args).await,
        Commands::ConnectTcp(args) => connect_tcp(args).await,
        Commands::ListenUdp(args) => listen_udp(args).await,
        Commands::ConnectUdp(args) => connect_udp(args).await,
        Commands::ListenWhip(args) => listen_whip(args).await,
        Commands::ConnectWhip(args) => connect_whip(args).await,

        #[cfg(unix)]
        Commands::ListenUnix(args) => listen_unix(args).await,

        #[cfg(unix)]
        Commands::ConnectUnix(args) => connect_unix(args).await,
    };
    match res {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            // `Display` prints only the outer error.  Include its sources so
            // connection failures report Iroh's actionable cause as well.
            eprintln!("error: {e:#}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(verbose: u8, verbose_max: u8) -> CommonArgs {
        CommonArgs {
            ipv4_addr: None,
            ipv6_addr: None,
            custom_alpn: None,
            verbose,
            verbose_max,
        }
    }

    #[test]
    fn verbosity_defaults_to_quiet() {
        assert_eq!(common(0, MAX_VERBOSE).verbosity(), 0);
        assert_eq!(log_level(0), Level::ERROR);
    }

    #[test]
    fn verbosity_maps_to_levels() {
        assert_eq!(common(1, MAX_VERBOSE).verbosity(), 1);
        assert_eq!(log_level(1), Level::INFO);
        assert_eq!(common(2, MAX_VERBOSE).verbosity(), 2);
        assert_eq!(log_level(2), Level::DEBUG);
        assert_eq!(common(3, MAX_VERBOSE).verbosity(), 3);
        assert_eq!(log_level(3), Level::TRACE);
    }

    #[test]
    fn verbose_max_caps_verbosity() {
        // -v -v -v with --verbose-max 1 is just info
        assert_eq!(common(3, 1).verbosity(), 1);
        assert_eq!(log_level(common(3, 1).verbosity()), Level::INFO);
        // --verbose-max above the maximum is clamped to the maximum
        assert_eq!(common(10, 100).verbosity(), MAX_VERBOSE);
        // --verbose-max 0 mutes everything
        assert_eq!(common(5, 0).verbosity(), 0);
        // a lower verbose count is unaffected by a higher cap
        assert_eq!(common(2, 3).verbosity(), 2);
    }

    #[test]
    fn stats_start_at_debug() {
        assert!(!stats_enabled(0));
        assert!(!stats_enabled(1));
        assert!(stats_enabled(2));
        assert!(stats_enabled(3));
    }
}
