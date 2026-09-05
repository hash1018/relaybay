//! Cutting media into packets small enough to cross a network.
//!
//! RTSP and WebRTC both carry media as RTP, and both need the same thing
//! done to it first: an access unit is one logical thing but can be a
//! hundred kilobytes, and a packet cannot be. So each one is cut up, and the
//! rules for cutting differ per codec — which is why there is a module here
//! per codec, the way there is under [`crate::codec`].
//!
//! # What every packet looks like
//!
//! ```text
//! 0                   1                   2                   3
//! |V=2|P|X|  CC  |M|      PT     |       sequence number         |
//! |                           timestamp                          |
//! |                             SSRC                             |
//! |                           payload …                          |
//! ```
//!
//! Twelve bytes, then the payload. Of the header fields only four are ever
//! anything but zero here, and three of them are bookkeeping:
//!
//! - **sequence number** counts packets, so a receiver can put them back in
//!   order and notice a gap. It wraps at 16 bits and that is expected.
//! - **timestamp** says when the media belongs, counted in the track's own
//!   clock — 90 kHz for H.264, the sample rate for AAC. Every packet of one
//!   access unit carries the same one, which is how a receiver knows they
//!   belong together.
//! - **SSRC** names the stream. One per track, so a receiver told about two
//!   tracks can tell their packets apart on one socket.
//! - **marker** is the only one that means something per codec. Here it says
//!   "this packet completes an access unit", which is what lets a receiver
//!   hand one to a decoder without waiting to see whether more is coming.

pub mod aac;
pub mod h264;

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};

/// The fixed part of an RTP header, which is all of it this writes.
pub const HEADER_SIZE: usize = 12;

/// How large a packet may be, header included.
///
/// Below the 1500 an Ethernet frame holds, with room for the IP and UDP
/// headers under it and a little more besides: a packet that has to be
/// fragmented by the network is a packet that is lost whole when any of its
/// fragments is.
pub const MTU: usize = 1400;

/// The payload types this hands out. Both are in the range a session is free
/// to assign, and the SDP a receiver is given says which is which.
pub mod payload_type {
    /// H.264.
    pub const VIDEO: u8 = 96;
    /// AAC.
    pub const AUDIO: u8 = 97;
}

/// What can be wrong with something being packetized.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RtpError {
    /// An AAC access unit longer than the thirteen bits its size is written
    /// in. At any bitrate anybody sends, a frame is a few hundred bytes.
    #[error("an access unit of {length} bytes does not fit the {limit} its size field holds")]
    AccessUnitTooLong { length: usize, limit: usize },

    /// A packet size with no room for a payload after the headers.
    #[error("an MTU of {0} leaves no room for a payload")]
    MtuTooSmall(usize),
}

/// One track's outgoing RTP stream: what a receiver needs to keep the
/// packets of it apart from every other's, and in order.
///
/// One per track. Two tracks sharing a sequence number space would look to a
/// receiver like one stream with half its packets missing.
#[derive(Clone, Debug)]
pub struct Stream {
    ssrc: u32,
    payload_type: u8,
    clock_rate: u32,
    sequence: u16,
    /// What the first packet's timestamp is counted from.
    ///
    /// Not zero, because a receiver is not supposed to be able to tell where
    /// a stream started from its timestamps, and because a stream that
    /// resumes after an interruption should not appear to go back in time.
    /// The value itself means nothing; only the differences do.
    origin: u32,
    mtu: usize,
}

impl Stream {
    /// A stream identified by `ssrc`, counting in `clock_rate` ticks.
    ///
    /// The first sequence number and timestamp are derived from the SSRC, so
    /// that two streams started at the same moment do not agree about
    /// either. Nothing here has to be unpredictable — RTP over RTSP is not
    /// protected by any of these — only different.
    pub fn new(ssrc: u32, payload_type: u8, clock_rate: u32) -> Self {
        let mixed = scramble(u64::from(ssrc));
        Self {
            ssrc,
            payload_type,
            clock_rate,
            sequence: mixed as u16,
            origin: (mixed >> 16) as u32,
            mtu: MTU,
        }
    }

    /// Sets how large a packet may be, header included.
    pub fn with_mtu(mut self, mtu: usize) -> Result<Self, RtpError> {
        if mtu <= HEADER_SIZE {
            return Err(RtpError::MtuTooSmall(mtu));
        }
        self.mtu = mtu;
        Ok(self)
    }

    /// Which stream this is, as its packets say.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// What its packets are, as the SDP has to say.
    pub fn payload_type(&self) -> u8 {
        self.payload_type
    }

    /// The clock its timestamps count in.
    pub fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    /// How many bytes of payload a packet has room for.
    fn budget(&self) -> usize {
        self.mtu - HEADER_SIZE
    }

    /// The timestamp a moment in the stream is written as.
    ///
    /// Wraps at 32 bits, which every receiver expects: at 90 kHz that is
    /// thirteen and a half hours, and a stream that runs longer has not
    /// started again.
    fn timestamp(&self, at: Duration) -> u32 {
        // In 128 bits because nanoseconds times a sample rate overflows 64
        // after a few years, and a stream is allowed to run that long.
        let ticks = at.as_nanos() * u128::from(self.clock_rate) / 1_000_000_000;
        self.origin.wrapping_add(ticks as u32)
    }

    /// Builds one packet out of a prefix the codec added and the media after
    /// it.
    fn packet(&mut self, marker: bool, timestamp: u32, prefix: &[u8], body: &[u8]) -> Bytes {
        let mut out = BytesMut::with_capacity(HEADER_SIZE + prefix.len() + body.len());
        // Version 2, no padding, no extension, no contributing sources.
        out.put_u8(0x80);
        out.put_u8((u8::from(marker) << 7) | self.payload_type);
        out.put_u16(self.sequence);
        out.put_u32(timestamp);
        out.put_u32(self.ssrc);
        out.put_slice(prefix);
        out.put_slice(body);
        self.sequence = self.sequence.wrapping_add(1);
        out.freeze()
    }
}

/// Spreads a value's bits about, for the starting numbers that only have to
/// differ. One step of splitmix64.
fn scramble(value: u64) -> u64 {
    let mut mixed = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

/// What a packet says about itself, for tests and for anything that has to
/// look at one it did not write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl Header {
    /// Reads the fixed header, or `None` if there is not one there.
    ///
    /// Only the fixed part: a packet with contributing sources or an
    /// extension has more in front of its payload, and nothing here writes
    /// one.
    pub fn parse(packet: &[u8]) -> Option<(Self, &[u8])> {
        let fixed = packet.get(..HEADER_SIZE)?;
        Some((
            Self {
                marker: fixed[1] & 0x80 != 0,
                payload_type: fixed[1] & 0x7f,
                sequence: u16::from_be_bytes([fixed[2], fixed[3]]),
                timestamp: u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]),
                ssrc: u32::from_be_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]),
            },
            &packet[HEADER_SIZE..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> Stream {
        Stream::new(0x1234_5678, payload_type::VIDEO, 90_000)
    }

    #[test]
    fn a_header_says_what_it_was_built_with() {
        let mut stream = stream();
        let packet = stream.packet(true, 900, &[0xaa], &[0xbb, 0xcc]);
        let (header, payload) = Header::parse(&packet).unwrap();

        assert!(header.marker);
        assert_eq!(header.payload_type, payload_type::VIDEO);
        assert_eq!(header.timestamp, 900);
        assert_eq!(header.ssrc, 0x1234_5678);
        assert_eq!(payload, &[0xaa, 0xbb, 0xcc]);
        // Version 2, and nothing else set.
        assert_eq!(packet[0], 0x80);
    }

    #[test]
    fn sequence_numbers_count_up_and_wrap() {
        let mut stream = stream();
        stream.sequence = u16::MAX - 1;
        let first = Header::parse(&stream.packet(false, 0, &[], &[])).unwrap().0;
        let second = Header::parse(&stream.packet(false, 0, &[], &[])).unwrap().0;
        let third = Header::parse(&stream.packet(false, 0, &[], &[])).unwrap().0;

        assert_eq!(first.sequence, u16::MAX - 1);
        assert_eq!(second.sequence, u16::MAX);
        assert_eq!(third.sequence, 0);
    }

    #[test]
    fn a_timestamp_counts_in_the_tracks_own_clock() {
        let video = Stream::new(1, payload_type::VIDEO, 90_000);
        let audio = Stream::new(1, payload_type::AUDIO, 44_100);
        let second = Duration::from_secs(1);

        // The origin is the same for both, since it comes from the SSRC, so
        // the difference is what each clock counts in a second.
        assert_eq!(
            video
                .timestamp(second)
                .wrapping_sub(video.timestamp(Duration::ZERO)),
            90_000
        );
        assert_eq!(
            audio
                .timestamp(second)
                .wrapping_sub(audio.timestamp(Duration::ZERO)),
            44_100
        );
    }

    #[test]
    fn a_timestamp_does_not_start_at_zero() {
        // A receiver is not meant to be able to read where a stream began
        // out of its first packet.
        let stream = Stream::new(0xdead_beef, payload_type::VIDEO, 90_000);
        assert_ne!(stream.timestamp(Duration::ZERO), 0);
    }

    #[test]
    fn two_streams_do_not_agree_about_where_they_started() {
        let first = Stream::new(1, payload_type::VIDEO, 90_000);
        let second = Stream::new(2, payload_type::VIDEO, 90_000);
        assert_ne!(first.sequence, second.sequence);
        assert_ne!(first.origin, second.origin);
    }

    #[test]
    fn a_timestamp_survives_a_stream_running_for_years() {
        // Nanoseconds times 90 000 overflows 64 bits after about six years,
        // which is not long for a camera.
        let stream = Stream::new(1, payload_type::VIDEO, 90_000);
        let decade = Duration::from_secs(10 * 365 * 24 * 60 * 60);
        let expected =
            (u128::from(stream.origin) + decade.as_nanos() * 90_000 / 1_000_000_000) as u32;
        assert_eq!(stream.timestamp(decade), expected);
    }

    #[test]
    fn a_packet_with_no_room_for_a_payload_is_refused() {
        assert_eq!(
            stream().with_mtu(HEADER_SIZE).unwrap_err(),
            RtpError::MtuTooSmall(HEADER_SIZE)
        );
        assert!(stream().with_mtu(HEADER_SIZE + 1).is_ok());
    }

    #[test]
    fn a_header_shorter_than_a_header_reads_as_nothing() {
        assert_eq!(Header::parse(&[0x80; HEADER_SIZE - 1]), None);
    }
}
