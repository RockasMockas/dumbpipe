//! SDP handling for the webrtc tunnel.
//!
//! The host is the media authority: it knows what the publisher (OBS) actually
//! offered and which payload types it actually sends, and it tells the viewer
//! about that in a [`SessionHeader`] over the tunnel. The viewer turns that
//! into a plain RTP SDP file for a media player, because neither mpv nor VLC
//! is a WebRTC client and both need a description of the stream before they can
//! open a UDP port.

use std::fmt::Write as _;

use crate::rtp::ParameterSets;

/// Audio or video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// Video.
    Video,
    /// Audio.
    Audio,
}

impl MediaKind {
    /// The name used in SDP and in the session header.
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }

    /// Parse the name used in SDP and in the session header.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "video" => Some(MediaKind::Video),
            "audio" => Some(MediaKind::Audio),
            _ => None,
        }
    }
}

/// What a publisher announced for one payload type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// The payload type.
    pub pt: u8,
    /// Codec name as in `a=rtpmap`, e.g. `H264`.
    pub codec: String,
    /// Clock rate in Hz.
    pub clock: u32,
    /// Number of channels, 1 for video.
    pub channels: u8,
    /// Format parameters as in `a=fmtp`, without `sprop-parameter-sets`.
    pub fmtp: String,
}

/// A media description of a publisher offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferMedia {
    /// Audio or video.
    pub kind: MediaKind,
    /// The announced payload types, in the order they appear in the m-line.
    ///
    /// Payload types that belong to a retransmission or redundancy codec (rtx,
    /// red, ulpfec, flexfec) are not included, they are never forwarded.
    pub payloads: Vec<Payload>,
}

/// Whether a codec name is a real media codec rather than a redundancy helper.
fn is_media_codec(codec: &str) -> bool {
    !matches!(
        codec.to_ascii_lowercase().as_str(),
        "rtx" | "red" | "ulpfec" | "flexfec" | "flexfec-03" | "cn" | "telephone-event" | "aac"
    )
}

/// Parse the media lines of an SDP offer.
///
/// This is a deliberately small parser: it only extracts what a plain RTP
/// player needs to know about the publisher's stream, namely for every audio or
/// video media line the payload types, their codec, clock rate and channels, and
/// their format parameters.
pub fn parse_offer(sdp: &str) -> Vec<OfferMedia> {
    let mut media: Vec<OfferMedia> = Vec::new();
    // The payload types of the current m-line, in order.
    let mut current: Option<(MediaKind, Vec<u8>)> = None;
    // rtpmap and fmtp of the current m-line, keyed by payload type.
    let mut rtpmap: Vec<(u8, String, u32, u8)> = Vec::new();
    let mut fmtp: Vec<(u8, String)> = Vec::new();

    fn flush(
        current: &Option<(MediaKind, Vec<u8>)>,
        rtpmap: &[(u8, String, u32, u8)],
        fmtp: &[(u8, String)],
        media: &mut Vec<OfferMedia>,
    ) {
        let Some((kind, pts)) = current else {
            return;
        };
        let payloads = pts
            .iter()
            .filter_map(|&pt| {
                let (_, codec, clock, channels) = rtpmap.iter().find(|(p, ..)| *p == pt)?.clone();
                if !is_media_codec(&codec) {
                    return None;
                }
                let fmtp = fmtp
                    .iter()
                    .find(|(p, _)| *p == pt)
                    .map(|(_, f)| strip_sprop(f))
                    .unwrap_or_default();
                Some(Payload {
                    pt,
                    codec,
                    clock,
                    channels,
                    fmtp,
                })
            })
            .collect();
        media.push(OfferMedia {
            kind: *kind,
            payloads,
        });
    }

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            flush(&current, &rtpmap, &fmtp, &mut media);
            current = None;
            rtpmap.clear();
            fmtp.clear();

            // `m=<media> <port> <proto> <fmt> ...`
            let mut fields = rest.split_whitespace();
            let kind = fields.next().and_then(MediaKind::parse);
            let _port = fields.next();
            let _proto = fields.next();
            let pts: Vec<u8> = fields.filter_map(|p| p.parse::<u8>().ok()).collect();
            let Some(kind) = kind else { continue };
            if pts.is_empty() {
                continue;
            }
            current = Some((kind, pts));
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // `<pt> <codec>/<clock>[/<channels>]`
            if let Some((pt, mapping)) = rest.split_once(' ') {
                let pt = pt.parse::<u8>().ok();
                let Some(pt) = pt else { continue };
                let mut fields = mapping.split('/');
                let codec = fields.next().unwrap_or_default().to_string();
                let clock = fields
                    .next()
                    .and_then(|c| c.parse::<u32>().ok())
                    .unwrap_or(90_000);
                let channels = fields
                    .next()
                    .and_then(|c| c.parse::<u8>().ok())
                    .unwrap_or(1);
                rtpmap.push((pt, codec, clock, channels));
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // `<pt> <params>`
            if let Some((pt, params)) = rest.split_once(' ') {
                if let Ok(pt) = pt.parse::<u8>() {
                    fmtp.push((pt, params.trim().to_string()));
                }
            }
        }
    }
    flush(&current, &rtpmap, &fmtp, &mut media);
    media
}

/// Remove `sprop-parameter-sets` from a format parameter string.
///
/// The parameter sets travel in their own fields of the session header, so the
/// viewer can update them without changing the negotiated format parameters.
fn strip_sprop(fmtp: &str) -> String {
    fmtp.split(';')
        .filter(|p| !p.trim().starts_with("sprop-parameter-sets"))
        .collect::<Vec<_>>()
        .join(";")
}

/// One media of the session header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaHeader {
    /// Audio or video.
    pub kind: MediaKind,
    /// The payload type the publisher actually sends.
    pub pt: u8,
    /// Codec name, e.g. `H264`.
    pub codec: String,
    /// Clock rate in Hz.
    pub clock: u32,
    /// Number of channels, 1 for video.
    pub channels: u8,
    /// Format parameters for the player SDP.
    pub fmtp: String,
    /// The H.264 parameter sets, if the codec is H.264.
    pub sets: ParameterSets,
}

/// What the host tells the viewer about the stream.
///
/// Sent as a text datagram, repeated periodically so that losing one does not
/// matter, and immediately whenever the stream changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    /// Generation counter, increased whenever the negotiated media changes.
    pub gen: u64,
    /// The live media, video first if present.
    pub media: Vec<MediaHeader>,
}

impl SessionHeader {
    /// Encode as the tunnel's text representation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("gen={}\n", self.gen);
        for m in &self.media {
            let _ = writeln!(
                out,
                "media {} pt={} codec={} clock={} channels={} fmtp={} sps={} pps={}",
                m.kind.as_str(),
                m.pt,
                m.codec,
                m.clock,
                m.channels,
                if m.fmtp.is_empty() { "-" } else { &m.fmtp },
                m.sets
                    .sps
                    .as_deref()
                    .map_or_else(|| "-".to_string(), |s| data_encoding::HEXLOWER.encode(s),),
                m.sets
                    .pps
                    .as_deref()
                    .map_or_else(|| "-".to_string(), |s| data_encoding::HEXLOWER.encode(s),),
            );
        }
        out.into_bytes()
    }

    /// Decode the tunnel's text representation.
    ///
    /// Returns `None` if the text is not a session header, so that a viewer can
    /// ignore a corrupted datagram and wait for the next one.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(buf).ok()?;
        let mut gen = None;
        let mut media = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else {
                continue;
            };
            if let Some(value) = first.strip_prefix("gen=") {
                gen = Some(value.parse().ok()?);
                continue;
            }
            if first != "media" {
                continue;
            }
            let kind = MediaKind::parse(fields.next()?)?;
            let mut pt = None;
            let mut codec = None;
            let mut clock = None;
            let mut channels = 1;
            let mut fmtp = String::new();
            let mut sps = None;
            let mut pps = None;
            for field in fields {
                let Some((key, value)) = field.split_once('=') else {
                    continue;
                };
                let value = if value == "-" { "" } else { value };
                match key {
                    "pt" => pt = value.parse().ok(),
                    "codec" => codec = Some(value.to_string()),
                    "clock" => clock = value.parse().ok(),
                    "channels" => channels = value.parse().unwrap_or(1),
                    "fmtp" => fmtp = value.to_string(),
                    "sps" => sps = decode_hex(value),
                    "pps" => pps = decode_hex(value),
                    _ => {}
                }
            }
            media.push(MediaHeader {
                kind,
                pt: pt?,
                codec: codec?,
                clock: clock?,
                channels,
                fmtp,
                sets: ParameterSets { sps, pps },
            });
        }
        Some(SessionHeader { gen: gen?, media })
    }

    /// The media of a kind, if any.
    pub fn media(&self, kind: MediaKind) -> Option<&MediaHeader> {
        self.media.iter().find(|m| m.kind == kind)
    }

    /// Whether the header describes at least one media.
    pub fn has_media(&self) -> bool {
        !self.media.is_empty()
    }
}

/// Decode a hex string, treating empty as absent.
fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() {
        None
    } else {
        data_encoding::HEXLOWER.decode(value.as_bytes()).ok()
    }
}

/// Render a plain RTP SDP for a media player.
///
/// * `video_port`: the local UDP port the video RTP is delivered to.
/// * `audio_port`: the local UDP port the audio RTP is delivered to. Media
///   players bind the RTCP port of each media as `port + 1`, so the two media
///   need two separate port pairs.
pub fn render_player_sdp(header: &SessionHeader, video_port: u16, audio_port: u16) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "v=0");
    let _ = writeln!(out, "o=dumbpipe {} 2 IN IP4 127.0.0.1", header.gen);
    let _ = writeln!(out, "s=dumbpipe");
    let _ = writeln!(out, "c=IN IP4 127.0.0.1");
    let _ = writeln!(out, "t=0 0");
    for m in &header.media {
        let port = match m.kind {
            MediaKind::Video => video_port,
            MediaKind::Audio => audio_port,
        };
        let _ = writeln!(out, "m={} {port} RTP/AVP {}", m.kind.as_str(), m.pt);
        let _ = writeln!(
            out,
            "a=rtpmap:{} {}/{}{}",
            m.pt,
            m.codec,
            m.clock,
            if m.kind == MediaKind::Audio && m.channels > 1 {
                format!("/{}", m.channels)
            } else {
                String::new()
            }
        );
        let mut fmtp = m.fmtp.clone();
        if let Some(sprop) = m.sets.to_sprop() {
            if !fmtp.is_empty() {
                fmtp.push(';');
            }
            fmtp.push_str("sprop-parameter-sets=");
            fmtp.push_str(&sprop);
        }
        if !fmtp.is_empty() {
            let _ = writeln!(out, "a=fmtp:{} {}", m.pt, fmtp);
        }
        let _ = writeln!(out, "a=recvonly");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
        o=- 4611731400498476682 2 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0 1\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=rtcp:9 IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:aBcD\r\n\
        a=rtcp-mux\r\n\
        a=rtpmap:96 H264/90000\r\n\
        a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
        a=rtpmap:97 rtx/90000\r\n\
        a=fmtp:97 apt=96\r\n\
        a=sendonly\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=fmtp:111 minptime=10;useinbandfec=1\r\n\
        a=rtpmap:63 red/48000/2\r\n\
        a=fmtp:63 111/111\r\n\
        a=sendonly\r\n";

    #[test]
    fn offer_is_parsed() {
        let media = parse_offer(OFFER);
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, MediaKind::Video);
        assert_eq!(media[0].payloads.len(), 1, "rtx is not forwarded");
        let video = &media[0].payloads[0];
        assert_eq!(video.pt, 96);
        assert_eq!(video.codec, "H264");
        assert_eq!(video.clock, 90_000);
        assert_eq!(
            video.fmtp,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        );

        assert_eq!(media[1].kind, MediaKind::Audio);
        assert_eq!(media[1].payloads.len(), 1, "red is not forwarded");
        let audio = &media[1].payloads[0];
        assert_eq!(audio.pt, 111);
        assert_eq!(audio.codec, "opus");
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.fmtp, "minptime=10;useinbandfec=1");
    }

    #[test]
    fn offer_sprop_is_stripped() {
        let sdp = "m=video 9 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
            a=fmtp:96 sprop-parameter-sets=Z0LAHw==,aM48gA==;packetization-mode=1\r\n";
        let media = parse_offer(sdp);
        assert_eq!(media[0].payloads[0].fmtp, "packetization-mode=1");
    }

    #[test]
    fn offer_without_media_is_empty() {
        assert!(parse_offer("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n").is_empty());
        // application media (data channels) is ignored
        assert!(parse_offer("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n").is_empty());
    }

    fn header() -> SessionHeader {
        SessionHeader {
            gen: 7,
            media: vec![
                MediaHeader {
                    kind: MediaKind::Video,
                    pt: 96,
                    codec: "H264".into(),
                    clock: 90_000,
                    channels: 1,
                    fmtp: "packetization-mode=1;profile-level-id=42e01f".into(),
                    sets: ParameterSets {
                        sps: Some(vec![0x67, 0x42, 0xc0, 0x1f]),
                        pps: Some(vec![0x68, 0xce, 0x3c, 0x80]),
                    },
                },
                MediaHeader {
                    kind: MediaKind::Audio,
                    pt: 111,
                    codec: "opus".into(),
                    clock: 48_000,
                    channels: 2,
                    fmtp: "minptime=10".into(),
                    sets: ParameterSets::default(),
                },
            ],
        }
    }

    #[test]
    fn session_header_roundtrips() {
        let header = header();
        let bytes = header.encode();
        let decoded = SessionHeader::decode(&bytes).expect("decodes");
        assert_eq!(decoded, header);
    }

    #[test]
    fn session_header_rejects_rubbish() {
        assert!(SessionHeader::decode(b"hello").is_none());
        assert!(SessionHeader::decode(b"gen=1\nmedia video pt=x\n").is_none());
        assert_eq!(SessionHeader::decode(b"gen=1\n").unwrap().media.len(), 0);
    }

    #[test]
    fn player_sdp_is_playable_shape() {
        let sdp = render_player_sdp(&header(), 5004, 5006);
        assert!(sdp.contains("c=IN IP4 127.0.0.1"));
        assert!(sdp.contains("m=video 5004 RTP/AVP 96"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000"));
        assert!(sdp.contains(
            "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f;sprop-parameter-sets=Z0LAHw==,aM48gA=="
        ));
        assert!(sdp.contains("m=audio 5006 RTP/AVP 111"));
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
        assert_eq!(sdp.matches("a=recvonly").count(), 2);
    }

    #[test]
    fn player_sdp_without_parameter_sets() {
        let mut header = header();
        header.media[0].sets = ParameterSets::default();
        let sdp = render_player_sdp(&header, 5004, 5006);
        assert!(sdp.contains("a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\n"));
        assert!(!sdp.contains("sprop"));
    }
}
