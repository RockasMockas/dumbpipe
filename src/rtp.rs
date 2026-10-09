//! RTP framing for the webrtc tunnel.
//!
//! The host side of [`crate::webrtc`] receives parsed RTP packets from str0m
//! (`Event::RtpPacket` hands out a parsed header and a headerless payload) and
//! forwards them verbatim to viewers over QUIC datagrams, so a viewer is a
//! plain UDP forwarder towards a local media player.
//!
//! Every datagram on the tunnel starts with a one byte tag:
//!
//! * [`TAG_SESSION`]: the session header, see [`crate::sdp::SessionHeader`].
//! * [`TAG_EPOCH`]: the next media packet is the start of a keyframe.
//! * [`TAG_VIDEO`]: a complete RTP packet for the video media.
//! * [`TAG_AUDIO`]: a complete RTP packet for the audio media.
//! * [`TAG_KEYFRAME_REQ`]: viewer to host, please ask the publisher for a
//!   keyframe.
//!
//! RTP header extensions and CSRCs are dropped when re-framing, the generated
//! player SDP advertises neither.

use data_encoding::{BASE64, HEXLOWER};

/// Session header, repeated periodically and on every change.
pub const TAG_SESSION: u8 = 0;
/// The next forwarded media packet starts a keyframe.
pub const TAG_EPOCH: u8 = 1;
/// An RTP packet of the video media.
pub const TAG_VIDEO: u8 = 2;
/// An RTP packet of the audio media.
pub const TAG_AUDIO: u8 = 3;
/// A viewer asking the host to request a keyframe from the publisher.
pub const TAG_KEYFRAME_REQ: u8 = 4;

/// The length of a fixed RTP header without extensions or CSRCs.
pub const RTP_HEADER_LEN: usize = 12;

/// The fields of an RTP header that we preserve when re-framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpInfo {
    /// RTP marker bit.
    pub marker: bool,
    /// Payload type.
    pub pt: u8,
    /// 16 bit sequence number as it appears on the wire.
    pub seq: u16,
    /// Media timestamp in the clock rate of the codec.
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
}

/// Serialize a minimal, self-contained RTP packet.
///
/// The header is written as `V=2, P=0, X=0, CC=0` plus the marker bit, payload
/// type, sequence number, timestamp and ssrc, followed by the payload. Header
/// extensions and CSRCs are deliberately not represented.
pub fn serialize_rtp(info: &RtpInfo, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    buf.push(0x80);
    buf.push((if info.marker { 0x80 } else { 0 }) | (info.pt & 0x7f));
    buf.extend_from_slice(&info.seq.to_be_bytes());
    buf.extend_from_slice(&info.timestamp.to_be_bytes());
    buf.extend_from_slice(&info.ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Parse the fixed part of an RTP header.
///
/// Returns the header fields and the offset of the payload, so a receiver can
/// route the packet by payload type and detect sequence number gaps. Header
/// extensions and CSRCs are skipped, padding is left in the payload.
///
/// Returns `None` if the buffer is not a version 2 RTP packet.
pub fn parse_header(buf: &[u8]) -> Option<(RtpInfo, usize)> {
    if buf.len() < RTP_HEADER_LEN || buf[0] >> 6 != 2 {
        return None;
    }
    let has_extension = buf[0] & 0x10 != 0;
    let csrc_count = (buf[0] & 0x0f) as usize;
    let mut offset = RTP_HEADER_LEN + csrc_count * 4;
    if offset > buf.len() {
        return None;
    }
    if has_extension {
        if offset + 4 > buf.len() {
            return None;
        }
        let words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        offset += 4 + words * 4;
        if offset > buf.len() {
            return None;
        }
    }
    let info = RtpInfo {
        marker: buf[1] & 0x80 != 0,
        pt: buf[1] & 0x7f,
        seq: u16::from_be_bytes([buf[2], buf[3]]),
        timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        ssrc: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
    };
    Some((info, offset))
}

/// Prefix a payload with a tunnel tag.
pub fn tagged(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(payload);
    buf
}

/// The codecs we know how to detect keyframes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 / AVC.
    H264,
    /// VP8.
    Vp8,
    /// Opus.
    Opus,
    /// Anything else, treated as always decodable.
    Other,
}

impl Codec {
    /// Map an SDP codec name to a [`Codec`].
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Codec::H264,
            "vp8" => Codec::Vp8,
            "opus" => Codec::Opus,
            _ => Codec::Other,
        }
    }
}

/// The H.264 parameter sets seen in a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSets {
    /// The most recent SPS, including the NAL header byte.
    pub sps: Option<Vec<u8>>,
    /// The most recent PPS, including the NAL header byte.
    pub pps: Option<Vec<u8>>,
}

impl ParameterSets {
    /// Whether both parameter sets are known.
    pub fn complete(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// Render the parameter sets as an SDP `sprop-parameter-sets` value.
    ///
    /// The NAL units are base64 encoded, separated by a comma, exactly as they
    /// appear on the wire (emulation prevention bytes included).
    pub fn to_sprop(&self) -> Option<String> {
        let sps = self.sps.as_deref()?;
        let pps = self.pps.as_deref()?;
        Some(format!("{},{}", BASE64.encode(sps), BASE64.encode(pps)))
    }

    /// Parse an `sprop-parameter-sets` value given as hex encoded bytes.
    ///
    /// The tunnel transports the parameter sets as hex to keep the session
    /// header free of base64 punctuation.
    pub fn from_sprop_hex(sps_hex: &str, pps_hex: &str) -> Self {
        let set = |hex: &str| -> Option<Vec<u8>> {
            if hex.is_empty() || hex == "-" {
                None
            } else {
                HEXLOWER.decode(hex.as_bytes()).ok()
            }
        };
        ParameterSets {
            sps: set(sps_hex),
            pps: set(pps_hex),
        }
    }

    /// The tunnel representation of the parameter sets.
    pub fn to_sprop_hex(&self) -> String {
        format!(
            "{}:{}",
            self.sps
                .as_deref()
                .map(|b| HEXLOWER.encode(b))
                .unwrap_or_default(),
            self.pps
                .as_deref()
                .map(|b| HEXLOWER.encode(b))
                .unwrap_or_default(),
        )
    }
}

/// H.264 nal unit types we care about.
mod nal {
    /// Coded slice of an IDR picture.
    pub const IDR: u8 = 5;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Single-time aggregation packet, type A.
    pub const STAP_A: u8 = 24;
    /// Fragmentation unit, type A.
    pub const FU_A: u8 = 28;
}

/// The nal unit type of a nal unit header byte.
fn nal_type(byte: u8) -> u8 {
    byte & 0x1f
}

/// Iterate over the nal units of an H.264 RTP payload.
///
/// Handles single nal units and STAP-A aggregation packets. FU-A fragments are
/// passed through as they are, they are handled by
/// [`h264_starts_keyframe`] directly.
fn for_each_nal(payload: &[u8], f: &mut impl FnMut(&[u8])) {
    let Some((&first, rest)) = payload.split_first() else {
        return;
    };
    if nal_type(first) != nal::STAP_A {
        f(payload);
        return;
    }
    let mut i = 0;
    while i + 2 <= rest.len() {
        let len = u16::from_be_bytes([rest[i], rest[i + 1]]) as usize;
        let start = i + 2;
        let end = start + len;
        if end > rest.len() {
            break;
        }
        f(&rest[start..end]);
        i = end;
    }
}

/// Whether an H.264 payload starts an independently decodable picture.
///
/// This is the case if the payload contains an SPS (parameter sets are repeated
/// at the start of every GOP by the encoders we target) or the first slice of
/// an IDR picture.
pub fn h264_starts_keyframe(payload: &[u8]) -> bool {
    let mut keyframe = false;
    for_each_nal(payload, &mut |nal| {
        let Some((&first, _)) = nal.split_first() else {
            return;
        };
        match nal_type(first) {
            nal::SPS | nal::IDR => keyframe = true,
            _ => {}
        }
    });
    if keyframe {
        return true;
    }
    // A fragmented IDR: the first fragment of an IDR nal unit.
    if let Some(&first) = payload.first() {
        if nal_type(first) == nal::FU_A && payload.len() > 1 {
            let fu = payload[1];
            return fu & 0x80 != 0 && nal_type(fu) == nal::IDR;
        }
    }
    false
}

/// Collect SPS and PPS nal units from an H.264 RTP payload.
pub fn scan_h264_parameter_sets(payload: &[u8], sets: &mut ParameterSets) {
    for_each_nal(payload, &mut |nal| {
        let Some((&first, _)) = nal.split_first() else {
            return;
        };
        match nal_type(first) {
            nal::SPS => sets.sps = Some(nal.to_vec()),
            nal::PPS => sets.pps = Some(nal.to_vec()),
            _ => {}
        }
    });
}

/// Whether a VP8 payload starts an independently decodable frame.
///
/// The descriptor payload type bit is not used, we only look at the partition
/// structure: a keyframe has `S=1` and `PID=0` in the 1 byte descriptor, and
/// the inverse keyframe flag in the extended descriptor must be unset.
pub fn vp8_starts_keyframe(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    // Bit 0 of the first descriptor octet is the inverse keyframe flag.
    let keyframe = first & 0x01 == 0;
    let extended = first & 0x80 != 0;
    if !extended {
        return keyframe;
    }
    // With X=1 there is one more descriptor octet, and the extended control
    // bits may add a picture id. None of that changes the keyframe flag.
    keyframe
}

/// Whether a payload starts an independently decodable access unit.
///
/// Used by the host to gate the media it forwards to a viewer: a viewer that
/// joins in the middle of a GOP only starts receiving at a keyframe, so the
/// player never has to decode a GOP tail without parameter sets.
pub fn starts_keyframe(payload: &[u8], codec: Codec) -> bool {
    match codec {
        Codec::H264 => h264_starts_keyframe(payload),
        Codec::Vp8 => vp8_starts_keyframe(payload),
        // Opus frames are independently decodable, and unknown codecs are
        // passed through ungated rather than stalling the stream.
        Codec::Opus | Codec::Other => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(ty: u8, body: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x60 | ty];
        buf.extend_from_slice(body);
        buf
    }

    #[test]
    fn rtp_roundtrip() {
        let info = RtpInfo {
            marker: true,
            pt: 96,
            seq: 0x1234,
            timestamp: 0xdeadbeef,
            ssrc: 0x01020304,
        };
        let payload = vec![1, 2, 3, 4];
        let buf = serialize_rtp(&info, &payload);
        assert_eq!(buf.len(), RTP_HEADER_LEN + 4);
        assert_eq!(buf[0], 0x80);
        let (parsed, offset) = parse_header(&buf).unwrap();
        assert_eq!(parsed, info);
        assert_eq!(offset, RTP_HEADER_LEN);
        assert_eq!(&buf[offset..], &payload);
    }

    #[test]
    fn parse_header_skips_extensions_and_csrc() {
        // V=2, X=1, CC=1: the fixed header, one csrc, a two byte profile with
        // one 32 bit word of extension data, then the payload.
        let mut buf = vec![
            0x91, 96, 0, 1, //
            0, 0, 0, 2, //
            0, 0, 0, 3, //
            0xde, 0xad, 0xbe, 0xef, // csrc
            0xbe, 0xe0, 0x00, 0x01, // extension header, one word of data
            0xca, 0xfe, 0xba, 0xbe, // extension data
        ];
        buf.extend_from_slice(b"payload");
        let (info, offset) = parse_header(&buf).unwrap();
        assert!(!info.marker);
        assert_eq!(info.pt, 96);
        assert_eq!(info.seq, 1);
        assert_eq!(offset, 24);
        assert_eq!(&buf[offset..], b"payload");

        // a truncated extension is not a packet
        let buf = &buf[..20];
        assert!(parse_header(buf).is_none());
    }

    #[test]
    fn parse_header_rejects_rubbish() {
        assert!(parse_header(b"hi").is_none());
        let mut buf = vec![0x00, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        assert!(parse_header(&buf).is_none());
        buf[0] = 0x80;
        assert!(parse_header(&buf).is_some());
        // csrc count beyond the buffer
        let buf = vec![0x8f, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        assert!(parse_header(&buf).is_none());
    }

    #[test]
    fn h264_single_idr_is_keyframe() {
        assert!(h264_starts_keyframe(&nal(nal::IDR, b"x")));
        assert!(!h264_starts_keyframe(&nal(1, b"x")));
        assert!(h264_starts_keyframe(&nal(nal::SPS, b"x")));
        assert!(!h264_starts_keyframe(&nal(nal::PPS, b"x")));
    }

    #[test]
    fn h264_stap_a_is_classified() {
        let sps = nal(nal::SPS, b"abc");
        let pps = nal(nal::PPS, b"de");
        let idr = nal(nal::IDR, b"frame");
        let mut buf = vec![nal::STAP_A];
        for nal in [&sps, &pps, &idr] {
            buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            buf.extend_from_slice(nal);
        }
        assert!(h264_starts_keyframe(&buf));

        let mut buf = vec![nal::STAP_A];
        let slice = nal(1, b"inter");
        buf.extend_from_slice(&(slice.len() as u16).to_be_bytes());
        buf.extend_from_slice(&slice);
        assert!(!h264_starts_keyframe(&buf));
    }

    #[test]
    fn h264_fu_a_start_only() {
        // start of an IDR fragment
        let start = vec![0x7c /* fu-a */, 0x85 /* S=1, type 5 */, 1, 2, 3];
        assert!(h264_starts_keyframe(&start));
        // continuation of an IDR fragment
        let cont = vec![0x7c, 0x45 /* S=0, E=1, type 5 */, 1, 2, 3];
        assert!(!h264_starts_keyframe(&cont));
        // start of a non-IDR fragment
        let inter = vec![0x7c, 0x81, 1, 2, 3];
        assert!(!h264_starts_keyframe(&inter));
    }

    #[test]
    fn h264_parameter_sets_are_collected() {
        let mut sets = ParameterSets::default();
        assert!(!sets.complete());
        scan_h264_parameter_sets(&nal(nal::SPS, b"sps"), &mut sets);
        assert!(!sets.complete());
        scan_h264_parameter_sets(&nal(nal::PPS, b"pps"), &mut sets);
        assert!(sets.complete());
        assert_eq!(sets.sps.as_deref(), Some(&nal(nal::SPS, b"sps")[..]));

        let sps = nal(nal::SPS, b"1");
        let pps = nal(nal::PPS, b"2");
        let mut buf = vec![nal::STAP_A];
        for nal in [&sps, &pps] {
            buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            buf.extend_from_slice(nal);
        }
        let mut sets = ParameterSets::default();
        scan_h264_parameter_sets(&buf, &mut sets);
        assert!(sets.complete());
        assert_eq!(sets.sps.as_deref(), Some(&sps[..]));
        assert_eq!(sets.pps.as_deref(), Some(&pps[..]));
    }

    #[test]
    fn sprop_hex_roundtrip() {
        let sets = ParameterSets {
            sps: Some(vec![0x67, 0x42, 0xc0, 0x1f]),
            pps: Some(vec![0x68, 0xce, 0x3c, 0x80]),
        };
        let hex = sets.to_sprop_hex();
        let (sps, pps) = hex.split_once(':').unwrap();
        let parsed = ParameterSets::from_sprop_hex(sps, pps);
        assert_eq!(parsed, sets);
        assert_eq!(
            sets.to_sprop().unwrap(),
            "Z0LAHw==,aM48gA==",
            "sprop must be standard base64"
        );
        assert!(ParameterSets::default().to_sprop().is_none());
    }

    #[test]
    fn vp8_keyframe_flag() {
        // S=1, PID=0, keyframe (inverse keyframe flag clear)
        assert!(vp8_starts_keyframe(&[0x10, 0x00, 0x00]));
        // inter frame
        assert!(!vp8_starts_keyframe(&[0x11, 0x00, 0x00]));
        // extended descriptor, X=1, still a keyframe
        assert!(vp8_starts_keyframe(&[0x90, 0x7f, 0x01, 0x02]));
        assert!(!vp8_starts_keyframe(&[0x91, 0x7f, 0x01, 0x02]));
    }

    #[test]
    fn opus_is_always_decodable() {
        assert!(starts_keyframe(b"", Codec::Opus));
        assert!(starts_keyframe(b"", Codec::Other));
        assert!(!starts_keyframe(&nal(1, b"x"), Codec::H264));
    }

    #[test]
    fn codec_names() {
        assert_eq!(Codec::from_name("H264"), Codec::H264);
        assert_eq!(Codec::from_name("h264"), Codec::H264);
        assert_eq!(Codec::from_name("VP8"), Codec::Vp8);
        assert_eq!(Codec::from_name("opus"), Codec::Opus);
        assert_eq!(Codec::from_name("L16"), Codec::Other);
    }

    #[test]
    fn tagging_prefixes() {
        assert_eq!(tagged(TAG_VIDEO, b"abc"), vec![TAG_VIDEO, b'a', b'b', b'c']);
    }
}
