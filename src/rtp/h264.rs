//! H.264 in RTP, as RFC 6184 packetizes it.
//!
//! A NAL unit and a packet are different sizes for different reasons. A NAL
//! unit is as long as a coded picture needs; a packet is as long as the
//! network will carry without breaking it up. So there are two shapes:
//!
//! - **A single NAL unit packet** is the unit, unchanged, as the payload.
//!   Its first byte is the NAL header, so a receiver reads what it is
//!   without being told.
//! - **FU-A** cuts one unit across packets. Two bytes go in front of each
//!   piece, holding between them everything the original header said, so a
//!   receiver can rebuild it — and the piece itself starts after that
//!   header, which is not sent again.
//!
//! ```text
//! FU-A:  |F|NRI| 28  |S|E|R| Type |  a piece of the unit …
//!         └── indicator ┘└─ header ┘
//! ```
//!
//! S marks the first piece and E the last. Rebuilding the original is
//! `(indicator & 0xe0) | (header & 0x1f)` followed by every piece in order.
//!
//! # What is not done
//!
//! STAP-A, which packs several small units into one packet. It saves a
//! header on parameter sets and little else, and every receiver that
//! supports it also supports the two shapes above. Not aggregating costs a
//! few packets per keyframe and removes a way to be wrong.

use std::time::Duration;

use bytes::Bytes;

use crate::codec::h264::Nal;
use crate::rtp::Stream;

/// The type a fragmented unit's packets carry instead of their own.
const FU_A: u8 = 28;

/// Cuts H.264 access units into packets.
///
/// One per track, because the [`Stream`] inside it is one track's sequence
/// numbers and nothing else's.
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
    /// Every packet gets the same timestamp, which is what says they are one
    /// picture, and only the last one gets the marker, which is what says
    /// there are no more of it coming.
    pub fn packetize(&mut self, nalus: &[Nal], at: Duration) -> Vec<Bytes> {
        let timestamp = self.stream.timestamp(at);
        let mut packets = Vec::with_capacity(nalus.len());
        for (index, nalu) in nalus.iter().enumerate() {
            let last = index + 1 == nalus.len();
            let data = nalu.data();
            if data.len() <= self.stream.budget() {
                packets.push(self.stream.packet(last, timestamp, &[], data));
            } else {
                self.fragment(&mut packets, last, timestamp, data);
            }
        }
        packets
    }

    /// Cuts one unit that will not fit into pieces that will.
    fn fragment(&mut self, packets: &mut Vec<Bytes>, last: bool, timestamp: u32, data: &[u8]) {
        // The three bits above the type say how important the unit is and
        // whether it is corrupt. They belong to the unit, so every piece
        // repeats them and the type goes in the second byte.
        let indicator = (data[0] & 0xe0) | FU_A;
        let kind = data[0] & 0x1f;
        // Two bytes of the budget go to the pair in front, and the original
        // header byte is not sent at all — it is rebuilt from them.
        let budget = self.stream.budget() - 2;
        let body = &data[1..];

        let pieces = body.len().div_ceil(budget);
        for (index, piece) in body.chunks(budget).enumerate() {
            let start = index == 0;
            let end = index + 1 == pieces;
            let header = (u8::from(start) << 7) | (u8::from(end) << 6) | kind;
            packets.push(
                self.stream
                    .packet(last && end, timestamp, &[indicator, header], piece),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{HEADER_SIZE, Header, MTU, RtpError, payload_type};

    fn packetizer() -> Packetizer {
        Packetizer::new(Stream::new(0x1234_5678, payload_type::VIDEO, 90_000))
    }

    fn nal(kind: u8, length: usize) -> Nal {
        let mut data = vec![0x60 | kind];
        data.extend((0..length - 1).map(|byte| byte as u8));
        Nal::new(Bytes::from_owner(data)).unwrap()
    }

    /// Puts a run of packets back into the NAL units they were made from,
    /// which is what a receiver does.
    fn reassemble(packets: &[Bytes]) -> Vec<Vec<u8>> {
        let mut nalus = Vec::new();
        let mut fragment: Vec<u8> = Vec::new();
        for packet in packets {
            let (_, payload) = Header::parse(packet).unwrap();
            if payload[0] & 0x1f != FU_A {
                nalus.push(payload.to_vec());
                continue;
            }
            let (indicator, header) = (payload[0], payload[1]);
            if header & 0x80 != 0 {
                // The start of one: rebuild the header the sender left out.
                fragment = vec![(indicator & 0xe0) | (header & 0x1f)];
            }
            fragment.extend_from_slice(&payload[2..]);
            if header & 0x40 != 0 {
                nalus.push(std::mem::take(&mut fragment));
            }
        }
        nalus
    }

    #[test]
    fn a_unit_that_fits_goes_in_one_packet_unchanged() {
        let sps = nal(7, 20);
        let packets = packetizer().packetize(std::slice::from_ref(&sps), Duration::ZERO);

        assert_eq!(packets.len(), 1);
        let (_, payload) = Header::parse(&packets[0]).unwrap();
        assert_eq!(payload, &sps.data()[..]);
    }

    #[test]
    fn a_unit_too_large_is_cut_up_and_comes_back_whole() {
        let idr = nal(5, MTU * 3 + 17);
        let packets = packetizer().packetize(std::slice::from_ref(&idr), Duration::ZERO);

        assert!(packets.len() > 3, "it was cut up");
        assert_eq!(reassemble(&packets), vec![idr.data().to_vec()]);
    }

    #[test]
    fn every_size_around_the_boundary_comes_back_whole() {
        // One byte either side of what fits, and of what fills a fragment
        // exactly: the places an off-by-one lives.
        let budget = MTU - HEADER_SIZE;
        for length in [
            1,
            2,
            budget - 1,
            budget,
            budget + 1,
            budget * 2 - 3,
            budget * 2 - 2,
            budget * 2 - 1,
        ] {
            let unit = nal(5, length);
            let packets = packetizer().packetize(std::slice::from_ref(&unit), Duration::ZERO);
            assert_eq!(reassemble(&packets), vec![unit.data().to_vec()], "{length}");
            assert!(
                packets.iter().all(|packet| packet.len() <= MTU),
                "{length}: a packet went over the MTU"
            );
        }
    }

    #[test]
    fn a_fragment_keeps_what_the_original_header_said() {
        // Reference idc 3, an IDR slice: both halves of the byte have to
        // survive being split across two.
        let mut data = vec![0x65];
        data.extend(std::iter::repeat_n(0xab, MTU * 2));
        let idr = Nal::new(Bytes::from_owner(data)).unwrap();

        let packets = packetizer().packetize(&[idr], Duration::ZERO);
        for packet in &packets {
            let (_, payload) = Header::parse(packet).unwrap();
            assert_eq!(payload[0], 0x60 | FU_A, "the importance bits are kept");
            assert_eq!(payload[1] & 0x1f, 5, "and the type");
        }
        assert_eq!(reassemble(&packets)[0][0], 0x65);
    }

    #[test]
    fn only_the_first_and_last_pieces_are_marked_as_such() {
        let packets = packetizer().packetize(&[nal(5, MTU * 3)], Duration::ZERO);
        let flags: Vec<_> = packets
            .iter()
            .map(|packet| {
                let (_, payload) = Header::parse(packet).unwrap();
                (payload[1] & 0x80 != 0, payload[1] & 0x40 != 0)
            })
            .collect();

        assert_eq!(flags.first(), Some(&(true, false)));
        assert_eq!(flags.last(), Some(&(false, true)));
        assert!(
            flags[1..flags.len() - 1]
                .iter()
                .all(|flag| *flag == (false, false))
        );
    }

    #[test]
    fn one_access_unit_is_one_timestamp_and_one_marker() {
        // A whole picture: parameter sets, then a slice big enough to be cut
        // up. A receiver has to see all of it as one moment, and be told
        // where it ends exactly once.
        let nalus = [nal(7, 20), nal(8, 8), nal(5, MTU * 2)];
        let packets = packetizer().packetize(&nalus, Duration::from_millis(1000));

        let headers: Vec<_> = packets
            .iter()
            .map(|packet| Header::parse(packet).unwrap().0)
            .collect();
        let first = headers[0].timestamp;
        assert!(headers.iter().all(|header| header.timestamp == first));
        assert_eq!(
            headers.iter().filter(|header| header.marker).count(),
            1,
            "one end, and it is the last packet"
        );
        assert!(headers.last().unwrap().marker);
    }

    #[test]
    fn pictures_a_second_apart_are_ninety_thousand_ticks_apart() {
        let mut packetizer = packetizer();
        let first = packetizer.packetize(&[nal(5, 10)], Duration::ZERO);
        let second = packetizer.packetize(&[nal(5, 10)], Duration::from_secs(1));

        let at = |packet: &Bytes| Header::parse(packet).unwrap().0.timestamp;
        assert_eq!(at(&second[0]).wrapping_sub(at(&first[0])), 90_000);
    }

    #[test]
    fn an_access_unit_with_nothing_in_it_makes_no_packets() {
        assert!(packetizer().packetize(&[], Duration::ZERO).is_empty());
    }

    #[test]
    fn a_tiny_mtu_still_cuts_correctly() {
        // Not a real network, but the fragment loop has to hold at the edge
        // as well as in the middle.
        let stream = Stream::new(1, payload_type::VIDEO, 90_000)
            .with_mtu(HEADER_SIZE + 3)
            .unwrap();
        let unit = nal(5, 20);
        let packets =
            Packetizer::new(stream).packetize(std::slice::from_ref(&unit), Duration::ZERO);
        assert_eq!(reassemble(&packets), vec![unit.data().to_vec()]);
    }

    #[test]
    fn a_stream_cannot_be_built_with_no_room_for_a_payload() {
        assert_eq!(
            Stream::new(1, payload_type::VIDEO, 90_000)
                .with_mtu(HEADER_SIZE)
                .unwrap_err(),
            RtpError::MtuTooSmall(HEADER_SIZE)
        );
    }
}
