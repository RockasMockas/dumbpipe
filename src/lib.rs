/// The ALPN for dumbpipe.
///
/// It is basically just passing data through 1:1, except that the connecting
/// side will send a fixed size handshake to make sure the stream is created.
pub const ALPN: &[u8] = b"DUMBPIPEV0";

/// The handshake to send when connecting.
///
/// The side that calls open_bi() first must send this handshake, the side that
/// calls accept_bi() must consume it.
pub const HANDSHAKE: [u8; 5] = *b"hello";

/// The ALPN used by the webrtc subcommands.
///
/// Deliberately different from the stream and udp ALPNs, so that a webrtc
/// viewer can never be mistaken for a stream or udp connector and vice versa.
pub const WEBRTC_ALPN: &[u8] = b"DUMBPIPE_WEBRTC_V0";

pub mod rtp;
pub mod sdp;
pub mod webrtc;
pub mod whip;

pub use iroh_tickets::endpoint::EndpointTicket;
