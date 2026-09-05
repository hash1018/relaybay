//! AAC in RTP, as RFC 3640 packetizes it in its `AAC-hbr` mode.
//!
//! An AAC access unit carries no length of its own — a decoder is told where
//! one ends by whatever framed it — so RTP has to say. Each packet opens
//! with a table of how long the units in it are:
//!
//! ```text
//! | AU-headers-length :16 | AU-header :16 | access unit … |
//!                          └ size :13 ┘└ index :3 ┘
//! ```
//!
//! `AU-headers-length` counts bits, not bytes, and with one unit per packet
//! it is always 16. The size is thirteen bits, which is the only real limit
//! here and one no bitrate anybody sends comes near.
//!
//! The three fields' widths are not fixed by the specification — they are
//! stated in the SDP, and this states `sizelength=13; indexlength=3;
//! indexdeltalength=3`, which is what everything else uses.
//!
//! # One unit to a packet
//!
//! The format allows several, and saves three bytes each time. A frame at
//! any ordinary bitrate is a few hundred bytes and arrives every twenty-odd
//! milliseconds, so aggregating would trade latency for a saving of about
//! one percent. Fragmenting is done, because a frame that does not fit has
//! to go somehow.

use std::time::Duration;

use bytes::Bytes;

use crate::rtp::{RtpError, Stream};

/// How many bytes go in front of the media: the header count, then one
/// header.
const PREFIX: usize = 4;

/// The largest access unit a thirteen-bit size can describe.
const MAX_ACCESS_UNIT: usize = (1 << 13) - 1;

/// Cuts AAC access units into packets.
#[derive(Clone, Debug)]
pub struct Packetizer {
    stream: Stream,
}

impl Packetizer {
    /// A packetizer writing onto `stream`.
    pub fn new(stream: Stream) -> Self {
        Self { stream }
    }

    /// The stream its packets belong to, which the SDP has to describe.
    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// Cuts one access unit into the packets that carry it.
    ///
    /// Almost always one. A frame too large for a packet is split, and every
    /// piece states the size of the whole so a receiver knows how much it is
    /// waiting for; only the last is marked.
    pub fn packetize(&mut self, frame: &Bytes, at: Duration) -> Result<Vec<Bytes>, RtpError> {
        if frame.len() > MAX_ACCESS_UNIT {
            return Err(RtpError::AccessUnitTooLong {
                length: frame.len(),
                limit: MAX_ACCESS_UNIT,
            });
        }
        let timestamp = self.stream.timestamp(at);
        // The header describes the whole unit even on a piece of one.
        let prefix = [
            0x00,
            0x10,
            (frame.len() >> 5) as u8,
            ((frame.len() & 0x1f) << 3) as u8,
        ];

        let budget = self.stream.budget() - PREFIX;
        let pieces = frame.len().div_ceil(budget).max(1);
        Ok((0..pieces)
            .map(|index| {
                let start = index * budget;
                let end = (start + budget).min(frame.len());
                let last = index + 1 == pieces;
                self.stream
                    .packet(last, timestamp, &prefix, &frame[start..end])
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{HEADER_SIZE, Header, MTU, payload_type};

    fn packetizer() -> Packetizer {
        Packetizer::new(Stream::new(0x1234_5678, payload_type::AUDIO, 44_100))
    }

    fn frame(length: usize) -> Bytes {
        Bytes::from_owner((0..length).map(|byte| byte as u8).collect::<Vec<_>>())
    }

    /// Reads the size a packet's header states, and the media after it.
    fn read(packet: &Bytes) -> (usize, &[u8]) {
        let (_, payload) = Header::parse(packet).unwrap();
        assert_eq!(
            u16::from_be_bytes([payload[0], payload[1]]),
            16,
            "one header, sixteen bits of it"
        );
        let header = u16::from_be_bytes([payload[2], payload[3]]);
        assert_eq!(header & 0x07, 0, "the first unit of a packet is index 0");
        (usize::from(header >> 3), &payload[PREFIX..])
    }

    #[test]
    fn a_frame_goes_in_one_packet_and_says_how_long_it_is() {
        let sound = frame(370);
        let packets = packetizer().packetize(&sound, Duration::ZERO).unwrap();

        assert_eq!(packets.len(), 1);
        let (stated, media) = read(&packets[0]);
        assert_eq!(stated, sound.len());
        assert_eq!(media, &sound[..]);
        assert!(Header::parse(&packets[0]).unwrap().0.marker);
    }

    #[test]
    fn every_size_around_the_boundary_comes_back_whole() {
        let budget = MTU - HEADER_SIZE - PREFIX;
        for length in [
            1,
            2,
            budget - 1,
            budget,
            budget + 1,
            budget * 2,
            MAX_ACCESS_UNIT,
        ] {
            let sound = frame(length);
            let packets = packetizer().packetize(&sound, Duration::ZERO).unwrap();

            let mut rebuilt = Vec::new();
            for packet in &packets {
                let (stated, media) = read(packet);
                assert_eq!(stated, length, "{length}: every piece states the whole");
                rebuilt.extend_from_slice(media);
                assert!(packet.len() <= MTU, "{length}: over the MTU");
            }
            assert_eq!(rebuilt, sound[..], "{length}");

            let markers = packets
                .iter()
                .filter(|packet| Header::parse(packet).unwrap().0.marker)
                .count();
            assert_eq!(markers, 1, "{length}: one end");
            assert!(
                Header::parse(packets.last().unwrap()).unwrap().0.marker,
                "{length}: and it is the last packet"
            );
        }
    }

    #[test]
    fn frames_a_second_apart_are_a_sample_rate_apart() {
        let mut packetizer = packetizer();
        let first = packetizer.packetize(&frame(100), Duration::ZERO).unwrap();
        let second = packetizer
            .packetize(&frame(100), Duration::from_secs(1))
            .unwrap();

        let at = |packet: &Bytes| Header::parse(packet).unwrap().0.timestamp;
        assert_eq!(at(&second[0]).wrapping_sub(at(&first[0])), 44_100);
    }

    #[test]
    fn a_frame_longer_than_its_size_field_is_refused() {
        // Nothing sends one: at 44.1 kHz a unit is 1024 samples, so this is
        // a bitrate of nearly three megabits for one channel of audio.
        let sound = frame(MAX_ACCESS_UNIT + 1);
        assert_eq!(
            packetizer().packetize(&sound, Duration::ZERO),
            Err(RtpError::AccessUnitTooLong {
                length: MAX_ACCESS_UNIT + 1,
                limit: MAX_ACCESS_UNIT
            })
        );
    }

    #[test]
    fn an_empty_frame_is_still_a_packet() {
        // Odd, but not malformed, and refusing one would drop a stream over
        // a single frame.
        let packets = packetizer()
            .packetize(&Bytes::new(), Duration::ZERO)
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(read(&packets[0]), (0, &[][..]));
    }
}
