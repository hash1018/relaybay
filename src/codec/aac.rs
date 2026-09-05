//! AAC: what a decoder has to be told before the frames mean anything.
//!
//! # The one record, under four names
//!
//! H.264 keeps its parameter sets in the stream and every protocol wraps
//! them differently. AAC does the opposite: there is one record, the
//! `AudioSpecificConfig`, and each protocol carries the same bytes somewhere
//! of its own — RTMP as the AAC sequence header, MP4 inside an `esds` box,
//! SDP as the hex `config` parameter, HLS by way of the MP4. So there is
//! nothing here that corresponds to [`crate::codec::h264::AvcConfig`]: the
//! neutral form and the wire form are the same bytes.
//!
//! Which is why [`Parameters`] keeps them verbatim and hands them back
//! unchanged. Only the first three fields are read, for the callers that
//! need the numbers — a sample rate for SDP's clock, an object type for an
//! HLS playlist's `mp4a.40.2`, a channel count for both — and everything
//! after them is carried without being understood. Re-encoding a record we
//! only partly read would drop whatever we had not learned to write.
//!
//! # What the frames are
//!
//! Raw access units, with no ADTS header. An ADTS header is a framing, the
//! way a start code is for H.264, and it belongs to the protocols that ask
//! for one rather than to the stream. See the crate docs.

use bytes::Bytes;

/// The sampling frequencies an index stands for. 13, 14 are reserved and 15
/// means the rate is written out in full instead.
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// How many channels a configuration stands for. 0 means the layout is
/// described further in, in a part of the record this does not read.
const CHANNELS: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 8];

/// What can be wrong with an `AudioSpecificConfig`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AacError {
    /// The record ended in the middle of a field.
    #[error("truncated at bit {offset}: {needed} more bit(s) needed")]
    Truncated { offset: usize, needed: usize },

    /// Object type 0 is the null codec, which decodes to silence and is not
    /// something a stream is published in.
    #[error("audio object type 0 is not a codec")]
    NoObjectType,

    /// A sampling frequency index the specification reserves.
    #[error("sampling frequency index {0} is reserved")]
    ReservedSampleRate(u8),

    /// A channel configuration of 0 says the layout is in a program config
    /// element further into the record, which this does not read. Refused
    /// rather than guessed at: every protocol downstream has to state a
    /// channel count, and a wrong one is not a thing a listener can correct
    /// for.
    #[error("the record does not state a channel count")]
    ChannelsNotStated,
}

/// What a decoder has to be given before any of the stream means anything.
///
/// The fields are private so that the numbers and the bytes can never
/// disagree: the numbers are read out of the record when it is parsed, and
/// the record cannot then be replaced under them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameters {
    config: Bytes,
    object_type: u8,
    sample_rate: u32,
    channels: u8,
}

impl Parameters {
    /// Reads an `AudioSpecificConfig` as RTMP's AAC sequence header, MP4's
    /// `esds` box and SDP's `config` parameter all carry it.
    pub fn parse(config: Bytes) -> Result<Self, AacError> {
        let mut bits = Bits {
            data: &config,
            offset: 0,
        };

        // Five bits, unless they are all ones, in which case six more follow
        // and count from 32. The escape exists because the field was defined
        // before there were more than 31 of them.
        let object_type = match bits.read(5)? {
            31 => bits.read(6)? + 32,
            value => value,
        };
        if object_type == 0 {
            return Err(AacError::NoObjectType);
        }

        // An index into the table of rates anything actually uses, or the
        // escape that says the rate is spelled out.
        let sample_rate = match bits.read(4)? {
            15 => bits.read(24)?,
            index => *SAMPLE_RATES
                .get(index as usize)
                .ok_or(AacError::ReservedSampleRate(index as u8))?,
        };

        let channels = *CHANNELS
            .get(bits.read(4)? as usize)
            .filter(|count| **count > 0)
            .ok_or(AacError::ChannelsNotStated)?;

        // Everything after this is the object type's own configuration —
        // frame length, core coder delay, the SBR and PS extensions — and is
        // carried without being read. See the module docs.
        Ok(Self {
            config,
            object_type: object_type as u8,
            sample_rate,
            channels,
        })
    }

    /// The record, unchanged, for the protocol that has to state it.
    pub fn config(&self) -> &Bytes {
        &self.config
    }

    /// Which AAC this is: 2 for the low-complexity profile every encoder
    /// sends, and what an HLS playlist writes after `mp4a.40.`.
    pub fn object_type(&self) -> u8 {
        self.object_type
    }

    /// Samples per second, which is also an RTP clock rate.
    ///
    /// For a stream using the SBR extension this is the rate of the base
    /// layer, which is the one RTP counts in and half of what comes out of a
    /// decoder. The extension is in the record and reaches every reader; it
    /// is only this number that describes the layer under it.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// How many channels, which every protocol downstream has to restate.
    pub fn channels(&self) -> u8 {
        self.channels
    }
}

/// Reads fields that do not fall on byte boundaries, which is most of what
/// an `AudioSpecificConfig` is made of.
struct Bits<'a> {
    data: &'a [u8],
    offset: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32, AacError> {
        let available = self.data.len() * 8;
        if self.offset + count > available {
            return Err(AacError::Truncated {
                offset: self.offset,
                needed: self.offset + count - available,
            });
        }
        let mut value = 0;
        for _ in 0..count {
            let byte = self.data[self.offset / 8];
            let bit = (byte >> (7 - self.offset % 8)) & 1;
            value = (value << 1) | u32::from(bit);
            self.offset += 1;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, BytesMut};

    use super::*;

    /// Packs `(value, width)` fields into a record, so that a test says what
    /// it is building rather than what the bytes came out as.
    fn record(fields: &[(u32, usize)]) -> Bytes {
        let mut bits = Vec::new();
        for (value, width) in fields {
            for shift in (0..*width).rev() {
                bits.push(((value >> shift) & 1) as u8);
            }
        }
        let mut out = BytesMut::new();
        for byte in bits.chunks(8) {
            out.put_u8(byte.iter().fold(0, |packed, bit| (packed << 1) | bit) << (8 - byte.len()));
        }
        out.freeze()
    }

    #[test]
    fn the_record_every_encoder_sends_reads_as_what_it_is() {
        // Two bytes an OBS or FFmpeg stream opens with, verbatim: AAC-LC,
        // 44.1 kHz, stereo.
        let parameters = Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap();
        assert_eq!(parameters.object_type(), 2);
        assert_eq!(parameters.sample_rate(), 44100);
        assert_eq!(parameters.channels(), 2);

        // And the one for 48 kHz.
        let parameters = Parameters::parse(Bytes::from_static(&[0x11, 0x90])).unwrap();
        assert_eq!(parameters.sample_rate(), 48000);
        assert_eq!(parameters.channels(), 2);
    }

    #[test]
    fn the_record_is_handed_back_exactly_as_it_arrived() {
        // Trailing bytes this does not read have to survive: they are what
        // an SBR extension is written in, and a reader downstream needs them
        // even though nothing here understands them.
        let config = Bytes::from_static(&[0x12, 0x10, 0x56, 0xe5, 0x00]);
        let parameters = Parameters::parse(config.clone()).unwrap();
        assert_eq!(parameters.config(), &config);
        assert_eq!(parameters.object_type(), 2);
    }

    #[test]
    fn every_sampling_frequency_index_reads_as_its_rate() {
        for (index, rate) in SAMPLE_RATES.iter().enumerate() {
            let config = record(&[(2, 5), (index as u32, 4), (2, 4)]);
            let parameters = Parameters::parse(config).expect("a defined index");
            assert_eq!(parameters.sample_rate(), *rate, "{index}");
        }
    }

    #[test]
    fn a_rate_outside_the_table_is_written_out_in_full() {
        let config = record(&[(2, 5), (15, 4), (44100, 24), (2, 4)]);
        let parameters = Parameters::parse(config).unwrap();
        assert_eq!(parameters.sample_rate(), 44100);
        assert_eq!(parameters.channels(), 2);
    }

    #[test]
    fn an_object_type_past_the_five_bits_uses_the_escape() {
        let config = record(&[(31, 5), (0, 6), (4, 4), (2, 4)]);
        let parameters = Parameters::parse(config).unwrap();
        assert_eq!(parameters.object_type(), 32);
        assert_eq!(parameters.sample_rate(), 44100);
    }

    #[test]
    fn every_channel_configuration_reads_as_its_count() {
        for (configuration, count) in CHANNELS.iter().enumerate().skip(1) {
            let config = record(&[(2, 5), (4, 4), (configuration as u32, 4)]);
            let parameters = Parameters::parse(config).expect("a stated layout");
            assert_eq!(parameters.channels(), *count, "{configuration}");
        }
    }

    #[test]
    fn a_reserved_sampling_frequency_index_is_refused() {
        for index in [13, 14] {
            let config = record(&[(2, 5), (index, 4), (2, 4)]);
            assert_eq!(
                Parameters::parse(config),
                Err(AacError::ReservedSampleRate(index as u8))
            );
        }
    }

    #[test]
    fn a_record_that_does_not_state_its_channels_is_refused() {
        let config = record(&[(2, 5), (4, 4), (0, 4)]);
        assert_eq!(Parameters::parse(config), Err(AacError::ChannelsNotStated));
    }

    #[test]
    fn the_null_object_type_is_refused() {
        let config = record(&[(0, 5), (4, 4), (2, 4)]);
        assert_eq!(Parameters::parse(config), Err(AacError::NoObjectType));
    }

    #[test]
    fn a_record_that_ends_mid_field_is_refused() {
        // One byte holds the five bits of object type and three of the four
        // the sampling frequency index needs.
        assert_eq!(
            Parameters::parse(Bytes::from_static(&[0x12])),
            Err(AacError::Truncated {
                offset: 5,
                needed: 1
            })
        );
        assert_eq!(
            Parameters::parse(Bytes::new()),
            Err(AacError::Truncated {
                offset: 0,
                needed: 5
            })
        );
    }
}
