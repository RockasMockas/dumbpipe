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
//! * [`TAG_FRAGMENT`]: a fragment of a datagram that was too large for a
//!   single QUIC datagram, see [`fragment_all`] and [`Reassembler`].
//!
//! RTP header extensions and CSRCs are dropped when re-framing, the generated
//! player SDP advertises neither.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

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
/// A fragment of a datagram that did not fit into a single QUIC datagram.
///
/// The host fragments such a datagram instead of dropping it: on a local
/// network the path MTU is large and nothing is ever fragmented, but across a
/// continent QUIC's datagram limit settles near 1100 bytes and full-size RTP
/// packets would otherwise be lost on every frame.
pub const TAG_FRAGMENT: u8 = 5;

/// The length of the fragment framing: tag, id, index, total.
pub const FRAG_HEADER_LEN: usize = 5;

/// How long a partial fragment set survives before it is discarded.
///
/// Realtime media that arrives later than this is useless anyway, and the
/// keyframe gate recovers the stream at the next IDR.
const FRAG_TIMEOUT: Duration = Duration::from_secs(2);

/// The number of datagrams the [`Reassembler`] tracks at once.
const FRAG_PENDING: usize = 8;

/// Split a datagram that is too large into fragments that fit into `max`
/// bytes each.
///
/// Returns `None` if the datagram cannot be framed (an absurd `max`) or would
/// need more fragments than the one byte index can count.
pub fn fragment_all(buf: &[u8], id: u16, max: usize) -> Option<Vec<Vec<u8>>> {
    if max <= FRAG_HEADER_LEN {
        return None;
    }
    let chunk = max - FRAG_HEADER_LEN;
    let total = buf.len().div_ceil(chunk);
    if total == 0 || total > u8::MAX as usize {
        return None;
    }
    Some(
        buf.chunks(chunk)
            .enumerate()
            .map(|(i, part)| fragment(id, i as u8, total as u8, part))
            .collect(),
    )
}

/// Frame one fragment of a datagram.
///
/// The framing is `tag, id(2), index(1), total(1)` followed by the chunk. The
/// id groups the fragments of one datagram, so a reassembler can track several
/// datagrams at once and tolerate fragments arriving out of order.
pub fn fragment(id: u16, index: u8, total: u8, chunk: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FRAG_HEADER_LEN + chunk.len());
    buf.push(TAG_FRAGMENT);
    buf.extend_from_slice(&id.to_be_bytes());
    buf.push(index);
    buf.push(total);
    buf.extend_from_slice(chunk);
    buf
}

/// The fragments collected for one datagram id.
struct Assembling {
    /// The number of fragments the datagram was split into.
    total: usize,
    /// The parts seen so far, `None` where a fragment is still missing.
    parts: Vec<Option<Vec<u8>>>,
    /// How many parts are filled.
    got: usize,
    /// When the first fragment of this datagram arrived.
    at: Instant,
}

/// Reassembles fragmented datagrams.
///
/// Fragments may arrive out of order and duplicates are ignored; a datagram is
/// yielded exactly once, when its last fragment arrives. Incomplete sets are
/// dropped after [`FRAG_TIMEOUT`] or when more than [`FRAG_PENDING`] datagrams
/// are being tracked: the missing fragments are then simply packet loss, and
/// the viewer's keyframe gate recovers from that.
#[derive(Default)]
pub struct Reassembler {
    pending: HashMap<u16, Assembling>,
}

impl Reassembler {
    /// Feed one fragment datagram (starting with [`TAG_FRAGMENT`]).
    ///
    /// Returns the complete datagram, including its original tag byte, when
    /// the last fragment of the datagram arrives.
    pub fn push(&mut self, data: &[u8], now: Instant) -> Option<Vec<u8>> {
        if data.len() < FRAG_HEADER_LEN || data[0] != TAG_FRAGMENT {
            return None;
        }
        let id = u16::from_be_bytes([data[1], data[2]]);
        let index = data[3] as usize;
        let total = data[4] as usize;
        if total == 0 || index >= total {
            return None;
        }
        self.evict(now);
        if !self.pending.contains_key(&id) && self.pending.len() >= FRAG_PENDING {
            self.drop_oldest();
        }
        let entry = self.pending.entry(id).or_insert_with(|| Assembling {
            total,
            parts: vec![None; total],
            got: 0,
            at: now,
        });
        if entry.total != total {
            // A reused id with a different framing, e.g. after the host's
            // MTU estimate changed: start over.
            *entry = Assembling {
                total,
                parts: vec![None; total],
                got: 0,
                at: now,
            };
        }
        if entry.parts[index].is_some() {
            return None;
        }
        entry.parts[index] = Some(data[FRAG_HEADER_LEN..].to_vec());
        entry.got += 1;
        if entry.got < total {
            return None;
        }
        let entry = self.pending.remove(&id)?;
        Some(entry.parts.into_iter().flatten().flatten().collect())
    }

    /// Discard all partial datagrams.
    ///
    /// Called at an epoch: fragments of the old GOP are useless now.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Drop entries that timed out.
    fn evict(&mut self, now: Instant) {
        self.pending.retain(|_, e| now.duration_since(e.at) < FRAG_TIMEOUT);
    }

    /// Drop the oldest entry to make room for a new one.
    fn drop_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .min_by_key(|(_, e)| e.at)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.pending.remove(&id);
        }
    }
}

/// The default how long a [`Reorder`] holds a packet waiting for a missing
/// earlier one, used when the viewer runs with no player buffer.
///
/// QUIC datagrams are unordered, so the viewer must put RTP packets back in
/// sequence order for the player. This is how long it waits for a hole to be
/// filled by a late packet before flushing what it has and letting the player
/// conceal the gap. Long enough to cover typical reordering, short enough that
/// the stream never visibly stalls.
///
/// When the user asks for a player buffer (`--buffer`) the viewer derives a
/// longer window from it instead, via [`Reorder::with_timeout`], so the two
/// reorder stages move in tandem. See [`reorder_timeout_for`].
const REORDER_TIMEOUT: Duration = Duration::from_millis(80);

/// Floor and ceiling for the reorder window derived from a player buffer.
///
/// The window is a fraction of the buffer (see [`reorder_timeout_for`]), kept
/// within these bounds: never below what covers ordinary reordering, never so
/// long that we sit on holes that are really lost packets.
const REORDER_TIMEOUT_MIN: Duration = Duration::from_millis(40);
const REORDER_TIMEOUT_MAX: Duration = Duration::from_millis(150);

/// Derive the viewer's reorder window from the player buffer.
///
/// `--buffer` is the player's total jitter/reorder budget; our [`Reorder`] is
/// the small, fast stage in front of it. We take a quarter of the buffer,
/// clamped to [`REORDER_TIMEOUT_MIN`]..=[`REORDER_TIMEOUT_MAX`], so a bigger
/// buffer nudges us toward catching worse long-distance reordering without
/// turning the reorder stage into a latency trap (a hole unfilled after ~150 ms
/// is a lost QUIC datagram, not a reordered one). `None` (no buffer, lowest
/// latency) keeps the default [`REORDER_TIMEOUT`].
pub fn reorder_timeout_for(buffer: Option<Duration>) -> Duration {
    buffer
        .map(|b| (b / 4).clamp(REORDER_TIMEOUT_MIN, REORDER_TIMEOUT_MAX))
        .unwrap_or(REORDER_TIMEOUT)
}

/// The most packets the [`Reorder`] holds per media at once.
///
/// A bound on memory: if reordering is so bad that this many packets pile up,
/// the stream is unusable anyway, so the held packets are flushed in order.
const REORDER_CAP: usize = 256;

/// Puts RTP packets back into sequence order.
///
/// The host forwards one RTP packet per QUIC datagram, and datagrams are
/// unreliable *and unordered*, so packets reach the viewer scrambled even on a
/// local network. Handing them to a player in arrival order makes it read the
/// reordering as packet loss: `missed N packets`, a thrashing jitter buffer and
/// corrupt pictures. The viewer runs one `Reorder` per media (video and audio
/// have separate sequence spaces) and emits packets strictly in sequence order,
/// so the player sees a clean, in-order stream.
///
/// A packet that arrives exactly when expected is emitted at once; a later one
/// is held until the hole before it is filled. If a hole is not filled within
/// the configured timeout ([`Reorder::default`]'s [`REORDER_TIMEOUT`], or a
/// longer window from [`Reorder::with_timeout`]), everything held is flushed in
/// sequence order and the hole is abandoned: the missing packet is genuine loss,
/// which the player conceals. Packets older than the next expected (very late,
/// or duplicates past a hole) are dropped.
pub struct Reorder {
    /// The next sequence number to emit, `None` until the first packet.
    next: Option<u16>,
    /// Held packets, `(seq, arrival, packet)`, out of order.
    held: Vec<(u16, Instant, Vec<u8>)>,
    /// How long to hold a hole before flushing. See [`Reorder::with_timeout`].
    timeout: Duration,
}

impl Default for Reorder {
    fn default() -> Self {
        Self {
            next: None,
            held: Vec::new(),
            timeout: REORDER_TIMEOUT,
        }
    }
}

impl Reorder {
    /// A reorder that waits `timeout` for a hole before flushing.
    ///
    /// Use [`reorder_timeout_for`] to size this from the player's `--buffer` so
    /// the two reorder stages stay in tandem.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Default::default()
        }
    }

    /// Feed one packet by its RTP sequence number.
    ///
    /// Returns the packets to hand to the player now, in sequence order: the
    /// packet itself if it closes the gap, plus any held packets it unblocks.
    pub fn push(&mut self, seq: u16, packet: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.next {
            None => {
                // First packet: it is the baseline, emit it and wait for the next.
                self.next = Some(seq.wrapping_add(1));
                out.push(packet);
            }
            Some(next) => {
                let ahead = seq.wrapping_sub(next) as i16;
                if ahead < 0 {
                    // Older than the next expected: too late, or a duplicate past
                    // a hole we already flushed. Drop it.
                    return out;
                }
                if ahead == 0 {
                    out.push(packet);
                    self.next = Some(next.wrapping_add(1));
                    self.drain(&mut out);
                } else {
                    // A future packet: hold it until the hole before it is filled.
                    if !self.held.iter().any(|(s, _, _)| *s == seq) {
                        if self.held.len() >= REORDER_CAP {
                            self.flush(&mut out);
                        }
                        self.held.push((seq, now, packet));
                    }
                    self.expire(now, &mut out);
                }
            }
        }
        out
    }

    /// Forget everything: called at an epoch, where the sequence baseline resets.
    pub fn clear(&mut self) {
        self.next = None;
        self.held.clear();
    }

    /// Emit held packets that are now consecutive with the next expected.
    fn drain(&mut self, out: &mut Vec<Vec<u8>>) {
        while let Some(next) = self.next {
            match self.held.iter().position(|(s, _, _)| *s == next) {
                Some(i) => {
                    let (s, _, p) = self.held.remove(i);
                    out.push(p);
                    self.next = Some(s.wrapping_add(1));
                }
                None => break,
            }
        }
    }

    /// Flush everything held if the oldest has waited past the timeout, so a
    /// lost packet can never stall the stream.
    fn expire(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        if self
            .held
            .iter()
            .map(|(_, at, _)| now.duration_since(*at))
            .max()
            .is_some_and(|wait| wait >= self.timeout)
        {
            self.flush(out);
        }
    }

    /// Release all held packets in sequence order, abandoning the holes.
    fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        let next = self.next.unwrap_or(0);
        self.held.sort_by_key(|(s, _, _)| s.wrapping_sub(next) as i16);
        for (s, _, p) in self.held.drain(..) {
            self.next = Some(s.wrapping_add(1));
            out.push(p);
        }
    }
}

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

    #[test]
    fn fragments_roundtrip() {
        // A 100 byte datagram split at a 40 byte limit is three fragments,
        // each within the limit, and they reassemble to exactly the original.
        let original: Vec<u8> = (0..100u8).collect();
        let frags = fragment_all(&original, 7, 40).expect("fragments");
        assert_eq!(frags.len(), 3);
        assert!(frags.iter().all(|f| f.len() <= 40));

        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.push(&frags[0], now).is_none());
        assert!(r.push(&frags[2], now).is_none(), "the last fragment is still missing");
        assert_eq!(r.push(&frags[1], now).expect("complete"), original);
    }

    #[test]
    fn fragments_tolerate_order_dupes_and_interleaving() {
        let original: Vec<u8> = (0..50u8).collect();
        let other: Vec<u8> = (200..250u8).collect();
        let frags = fragment_all(&original, 1, 25).expect("fragments");
        let other_frags = fragment_all(&other, 2, 25).expect("fragments");
        assert_eq!(frags.len(), 3);

        let mut r = Reassembler::default();
        let now = Instant::now();
        // out of order and interleaved with another datagram, with a dupe
        assert!(r.push(&other_frags[0], now).is_none());
        assert!(r.push(&frags[2], now).is_none());
        assert!(r.push(&frags[2], now).is_none(), "a dupe is not complete twice");
        assert!(r.push(&other_frags[1], now).is_none());
        assert!(r.push(&frags[0], now).is_none());
        assert_eq!(r.push(&frags[1], now).expect("complete"), original);
        assert_eq!(r.push(&other_frags[1], now), None, "the dupe completed nothing");
        assert_eq!(r.push(&other_frags[2], now).expect("complete"), other);
    }

    #[test]
    fn stale_fragments_are_dropped() {
        let frags = fragment_all(b"0123456789", 1, 8).expect("fragments");
        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.push(&frags[0], now).is_none());
        // past the timeout the partial set is gone, and the remaining
        // fragments must not complete a lie
        assert!(r.push(&frags[1], now + FRAG_TIMEOUT).is_none());
        assert!(r.push(&frags[2], now + FRAG_TIMEOUT).is_none());
        assert!(
            r.push(&frags[3], now + FRAG_TIMEOUT).is_none(),
            "the evicted first fragment must not be forgotten as present"
        );
    }

    #[test]
    fn clear_forgets_partial_datagrams() {
        // 10 bytes at a 8 byte limit is four fragments of three bytes.
        let frags = fragment_all(b"0123456789", 1, 8).expect("fragments");
        assert_eq!(frags.len(), 4);
        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.push(&frags[0], now).is_none());
        r.clear();
        // after the clear the old fragment is gone and must not count:
        assert!(r.push(&frags[1], now).is_none());
        assert!(r.push(&frags[2], now).is_none());
        assert!(r.push(&frags[3], now).is_none(), "fragment 0 was cleared");
        // ... and the datagram completes only when it arrives again
        assert_eq!(r.push(&frags[0], now).expect("complete"), b"0123456789");
    }

    #[test]
    fn fragment_rejects_absurd_limits() {
        assert!(fragment_all(b"12345", 0, FRAG_HEADER_LEN).is_none());
        assert!(fragment_all(&vec![0u8; 300], 0, 6).is_none(), "total > 255");
    }

    #[test]
    fn reorder_emits_in_order_packets_at_once() {
        let mut r = Reorder::default();
        let now = Instant::now();
        assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
        assert_eq!(r.push(2, vec![2], now), vec![vec![2]]);
        assert_eq!(r.push(3, vec![3], now), vec![vec![3]]);
    }

    #[test]
    fn reorder_holds_a_head_until_the_hole_is_filled() {
        let mut r = Reorder::default();
        let now = Instant::now();
        assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
        // 3 arrives before 2: hold it, emit nothing.
        assert!(r.push(3, vec![3], now).is_empty(), "2 is still missing");
        // 2 arrives late: it and the held 3 go out in order.
        assert_eq!(r.push(2, vec![2], now), vec![vec![2], vec![3]]);
        assert_eq!(r.push(4, vec![4], now), vec![vec![4]]);
    }

    #[test]
    fn reorder_flushes_held_packets_after_the_timeout() {
        let mut r = Reorder::default();
        let t0 = Instant::now();
        assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
        // 2 is genuinely lost; 3 and 4 pile up behind the hole.
        assert!(r.push(3, vec![3], t0).is_empty());
        assert!(r.push(4, vec![4], t0).is_empty());
        // once the oldest held packet has waited past the timeout, the next
        // packet flushes everything in order and abandons the hole at 2.
        let later = t0 + REORDER_TIMEOUT;
        assert_eq!(r.push(5, vec![5], later), vec![vec![3], vec![4], vec![5]]);
        // the stream resumes normally from the new baseline.
        assert_eq!(r.push(6, vec![6], later), vec![vec![6]]);
    }

    #[test]
    fn reorder_drops_packets_past_a_flushed_hole() {
        let mut r = Reorder::default();
        let t0 = Instant::now();
        assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
        assert!(r.push(3, vec![3], t0).is_empty());
        // the hole at 2 is abandoned after the timeout
        assert_eq!(r.push(4, vec![4], t0 + REORDER_TIMEOUT), vec![vec![3], vec![4]]);
        // the very late 2 must not be emitted out of order now
        assert!(r.push(2, vec![2], t0 + REORDER_TIMEOUT).is_empty(), "too late");
    }

    #[test]
    fn reorder_clear_resets_the_baseline() {
        let mut r = Reorder::default();
        let now = Instant::now();
        assert_eq!(r.push(5, vec![5], now), vec![vec![5]]);
        assert!(r.push(7, vec![7], now).is_empty());
        r.clear();
        // after an epoch the baseline is gone: a new low sequence number is the
        // start of a fresh GOP, not a late packet to drop.
        assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
    }

    #[test]
    fn reorder_timeout_tracks_the_player_buffer() {
        // No buffer (lowest latency): the default window.
        assert_eq!(reorder_timeout_for(None), REORDER_TIMEOUT);
        // A quarter of the buffer, clamped to the floor and ceiling.
        assert_eq!(
            reorder_timeout_for(Some(Duration::from_millis(200))),
            Duration::from_millis(50)
        );
        assert_eq!(
            reorder_timeout_for(Some(Duration::from_millis(500))),
            Duration::from_millis(125)
        );
        assert_eq!(
            reorder_timeout_for(Some(Duration::from_millis(1000))),
            REORDER_TIMEOUT_MAX,
            "capped so we never sit on lost packets"
        );
        assert_eq!(
            reorder_timeout_for(Some(Duration::from_millis(40))),
            REORDER_TIMEOUT_MIN,
            "floored to cover ordinary reordering"
        );
    }

    #[test]
    fn reorder_honours_a_custom_timeout() {
        let mut r = Reorder::with_timeout(Duration::from_millis(200));
        let t0 = Instant::now();
        assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
        assert!(r.push(3, vec![3], t0).is_empty());
        // still held at 80ms: the default window would have flushed by now.
        assert!(r.push(4, vec![4], t0 + Duration::from_millis(80)).is_empty());
        // flushed once past the longer, custom window.
        assert_eq!(
            r.push(5, vec![5], t0 + Duration::from_millis(200)),
            vec![vec![3], vec![4], vec![5]]
        );
    }
}
