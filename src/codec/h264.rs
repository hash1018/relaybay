//! H.264: the two framings a NAL unit stream arrives in, and the parameter
//! sets that have to reach a decoder before any of it means anything.
//!
//! # The two framings
//!
//! A NAL unit stream is a list of units; a *framing* is how that list is
//! written as one run of bytes. There are two in use, and which one a
//! payload carries is decided by whoever handed it over, never by the codec:
//!
//! - **Annex-B** separates units with a start code (`00 00 01`, usually
//!   written with a leading zero as `00 00 00 01`). A reader scans for the
//!   pattern, so it can join a stream anywhere. MPEG-TS, RTSP's SDP
//!   parameter sets and most decoder inputs use it.
//! - **Length-prefixed** puts each unit's byte count in front of it. A
//!   reader seeks instead of scanning, at the cost of having to start from
//!   the beginning. MP4 and RTMP use it, and both describe the stream with
//!   an [`AvcConfig`] record that says how wide the prefix is.
//!
//! Neither is more correct, and the NAL units are byte-identical under both.
//! This module converts in either direction and nothing else.
//!
//! # Where the parameter sets are
//!
//! An SPS and a PPS say what resolution, profile and coding tools a stream
//! uses. Without them a decoder cannot start, and every protocol solves that
//! differently: RTMP and MP4 keep them in an [`AvcConfig`] outside the
//! stream, MPEG-TS repeats them in front of every keyframe, RTSP sends them
//! once in SDP. So what a relay has to hold is the sets themselves —
//! [`Parameters`] — and be able to write them in whichever of those shapes
//! the next protocol wants.

use bytes::{BufMut, Bytes, BytesMut};

/// The four-byte start code. The three-byte form is equally valid and is
/// recognized on input, but nothing here has a reason to emit it.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// What a NAL unit is, from the `nal_unit_type` in its header byte.
///
/// These numbers mean this only in H.264. The same five bits in HEVC are six
/// bits at a different offset naming different things — 5 is an IDR slice
/// here and a reserved VCL type there — which is why this enum belongs to
/// this module rather than to the units a relay passes around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NalType {
    /// A slice of a picture that is not an IDR.
    ///
    /// Usually a P or a B, and sometimes an I: which of the three lives in
    /// the slice header, behind an exponential-Golomb code nothing here
    /// reads. Being an I is not enough to start a reader at anyway — see
    /// [`NalType::Idr`].
    Slice,
    /// A slice of an IDR picture, which is where a reader can be started.
    ///
    /// Not merely a picture coded without reference to any other — an I
    /// picture is that too — but one that no later picture may reference
    /// across, so everything after it decodes with nothing before it. That
    /// is the property a relay needs, and the only one of the two that can
    /// be read without decoding: it is these five bits, where the slice type
    /// is a field in a header this never parses.
    Idr,
    /// Supplemental enhancement information.
    Sei,
    /// A sequence parameter set.
    Sps,
    /// A picture parameter set.
    Pps,
    /// An access unit delimiter.
    Aud,
    /// Anything else, kept verbatim: a relay forwards what it does not
    /// recognize rather than dropping it.
    Other(u8),
}

impl NalType {
    /// Reads the type out of a unit's header byte. The three bits above it
    /// are the forbidden-zero bit and `nal_ref_idc`, neither of which
    /// identifies a unit.
    pub fn from_header(byte: u8) -> Self {
        match byte & 0x1f {
            1 => Self::Slice,
            5 => Self::Idr,
            6 => Self::Sei,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::Aud,
            other => Self::Other(other),
        }
    }

    /// The `nal_unit_type` this stands for.
    pub fn value(self) -> u8 {
        match self {
            Self::Slice => 1,
            Self::Idr => 5,
            Self::Sei => 6,
            Self::Sps => 7,
            Self::Pps => 8,
            Self::Aud => 9,
            Self::Other(value) => value,
        }
    }
}

/// One NAL unit, unframed: neither a start code nor a length prefix, just
/// the unit's own bytes beginning at its header byte.
///
/// The fields are private so that the two can never disagree — the kind is
/// read out of the data when the unit is made, and the data cannot then be
/// replaced under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nal {
    kind: NalType,
    data: Bytes,
}

impl Nal {
    /// Takes `data` as one unframed NAL unit, or `None` for an empty one:
    /// every unit has at least the header byte its type is read from.
    pub fn new(data: Bytes) -> Option<Self> {
        let kind = NalType::from_header(*data.first()?);
        Some(Self { kind, data })
    }

    /// What this unit is.
    pub fn kind(&self) -> NalType {
        self.kind
    }

    /// The unit's bytes, header byte first.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// The unit's bytes, for a caller assembling a payload out of them.
    pub fn into_data(self) -> Bytes {
        self.data
    }
}

/// What can be wrong with a payload that claims to be H.264.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum H264Error {
    /// A length prefix or record field ran past the end of its buffer.
    #[error("truncated at byte {offset}: {needed} more byte(s) needed")]
    Truncated { offset: usize, needed: usize },

    /// A NAL unit declared a length of zero, which no unit can have — it
    /// would carry not even a header byte.
    #[error("zero-length NAL unit at byte {offset}")]
    EmptyNalUnit { offset: usize },

    /// A NAL unit too long for the length prefix that has to describe it.
    #[error("NAL unit of {length} bytes does not fit a {size}-byte length prefix")]
    NalUnitTooLong { length: usize, size: usize },

    /// The first byte of an `AVCDecoderConfigurationRecord` is a version
    /// number, and 1 is the only one defined.
    #[error("unsupported AVC configuration record version {0}")]
    UnsupportedConfigVersion(u8),

    /// A record with no SPS or no PPS configures nothing.
    #[error("AVC configuration record carries no {0}")]
    MissingParameterSet(&'static str),
}

/// Whether an access unit can be decoded with nothing before it.
///
/// Reads every unit rather than the first: an access unit routinely opens
/// with an SEI or an access unit delimiter, and stopping at that one would
/// call a keyframe an ordinary picture.
pub fn is_keyframe(nalus: &[Nal]) -> bool {
    nalus.iter().any(|nalu| nalu.kind() == NalType::Idr)
}

/// Whether `data` opens with a start code, which is how a payload's framing
/// is told from the one its source declared.
///
/// A length-prefixed unit cannot begin this way: three leading zero bytes
/// would be a length of at least 16 MiB whose next byte is 1.
pub fn starts_with_start_code(data: &[u8]) -> bool {
    data.starts_with(&START_CODE) || data.starts_with(&START_CODE[1..])
}

/// Splits an Annex-B byte stream into its NAL units, discarding the start
/// codes.
///
/// Never fails. A start code can be preceded by anything and a run of them
/// can be empty; both are skipped rather than refused, because a scan that
/// gave up on the first oddity would be useless for the job Annex-B exists
/// for — joining a stream part-way through.
pub fn split_annex_b(data: &Bytes) -> Vec<Nal> {
    // Where each start code begins and how wide it is. Both are needed: the
    // unit starts after its own code and ends where the next one *begins*,
    // and the two codes need not be the same width.
    let mut codes: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let width = if data[offset..].starts_with(&START_CODE) {
            4
        } else if data[offset..].starts_with(&START_CODE[1..]) {
            3
        } else {
            offset += 1;
            continue;
        };
        codes.push((offset, width));
        offset += width;
    }
    codes
        .iter()
        .enumerate()
        .filter_map(|(index, (start, width))| {
            let unit = start + width;
            let end = codes
                .get(index + 1)
                .map(|(next, _)| *next)
                .unwrap_or(data.len());
            (unit < end).then(|| data.slice(unit..end))
        })
        .filter_map(Nal::new)
        .collect()
}

/// Splits a length-prefixed payload into its NAL units.
///
/// Fails rather than returning what it managed to read: a prefix size that
/// does not match the one the stream declared reads lengths out of picture
/// data, and the units that come back are neither an error nor a stream —
/// they look well formed and decode to nothing.
pub fn split_length_prefixed(data: &Bytes, nal_length_size: usize) -> Result<Vec<Nal>, H264Error> {
    let mut nalus = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let Some(prefix) = data.get(offset..offset + nal_length_size) else {
            return Err(H264Error::Truncated {
                offset,
                needed: nal_length_size - (data.len() - offset),
            });
        };
        let length = prefix
            .iter()
            .fold(0usize, |value, byte| (value << 8) | usize::from(*byte));
        offset += nal_length_size;
        if length == 0 {
            return Err(H264Error::EmptyNalUnit { offset });
        }
        let end = offset + length;
        if end > data.len() {
            return Err(H264Error::Truncated {
                offset,
                needed: end - data.len(),
            });
        }
        let nalu = Nal::new(data.slice(offset..end)).ok_or(H264Error::EmptyNalUnit { offset })?;
        nalus.push(nalu);
        offset = end;
    }
    Ok(nalus)
}

/// Writes NAL units as an Annex-B byte stream.
pub fn to_annex_b(nalus: &[Nal]) -> Bytes {
    let size: usize = nalus
        .iter()
        .map(|nalu| nalu.data().len() + START_CODE.len())
        .sum();
    let mut out = BytesMut::with_capacity(size);
    for nalu in nalus {
        out.put_slice(&START_CODE);
        out.put_slice(nalu.data());
    }
    out.freeze()
}

/// Writes NAL units with a length prefix each.
///
/// A unit too long for the prefix is refused rather than truncated: a
/// wrapped length points its reader into the middle of a picture, which is
/// the one failure that produces a stream nothing can detect.
pub fn to_length_prefixed(nalus: &[Nal], nal_length_size: usize) -> Result<Bytes, H264Error> {
    let size: usize = nalus
        .iter()
        .map(|nalu| nalu.data().len() + nal_length_size)
        .sum();
    let mut out = BytesMut::with_capacity(size);
    for nalu in nalus {
        put_length_prefixed(&mut out, nalu.data(), nal_length_size)?;
    }
    Ok(out.freeze())
}

fn put_length_prefixed(
    out: &mut BytesMut,
    data: &Bytes,
    nal_length_size: usize,
) -> Result<(), H264Error> {
    // A 4-byte prefix holds any length a `usize` can express on a 32-bit
    // target, so the limit is only meaningful for the narrower ones.
    let fits =
        nal_length_size >= size_of::<usize>() || data.len() < 1usize << (8 * nal_length_size);
    if !fits {
        return Err(H264Error::NalUnitTooLong {
            length: data.len(),
            size: nal_length_size,
        });
    }
    for shift in (0..nal_length_size).rev() {
        out.put_u8((data.len() >> (8 * shift)) as u8);
    }
    out.put_slice(data);
    Ok(())
}

/// What a decoder has to be given before any of the stream means anything.
///
/// This is the whole of what a protocol has to state about an H.264 track,
/// and it says nothing about how that protocol writes it down. RTSP puts it
/// in SDP, MPEG-TS repeats it in front of every keyframe, RTMP and MP4 wrap
/// it in an [`AvcConfig`]; all three are describing this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameters {
    /// Sequence parameter sets. At least one to be usable; the first is the
    /// one whose profile and level describe the stream.
    pub sps: Vec<Nal>,
    /// Picture parameter sets. At least one to be usable.
    pub pps: Vec<Nal>,
}

impl Parameters {
    /// The profile, constraint and level bytes, which name what a decoder
    /// must be able to do. Read from the first SPS, which is where a decoder
    /// reads them in any case.
    ///
    /// Every protocol restates them somewhere — an `AvcConfig`'s header,
    /// SDP's `profile-level-id`, an HLS playlist's `avc1.42c01e` — and all
    /// of those are this.
    pub fn profile_level(&self) -> Result<&[u8], H264Error> {
        let first = self
            .sps
            .first()
            .ok_or(H264Error::MissingParameterSet("SPS"))?
            .data();
        first.get(1..4).ok_or_else(|| H264Error::Truncated {
            offset: 1,
            needed: 4 - first.len(),
        })
    }
}

/// An `AVCDecoderConfigurationRecord`: how RTMP and MP4 write [`Parameters`]
/// down, and the width of the length prefix their payloads use.
///
/// The two arrive together because both are read from this one record — a
/// source that hands it over has, by that act, declared both — but they are
/// separate facts and only one of them is about the stream. The prefix width
/// belongs to whoever is doing the framing, and stops there; an egress that
/// frames some other way has no use for it. Which is why it is here rather
/// than in [`Parameters`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfig {
    /// What the record says a decoder needs.
    pub parameters: Parameters,
    /// How many bytes each NAL unit's length prefix occupies: 1, 2 or 4.
    pub nal_length_size: usize,
}

impl AvcConfig {
    /// Reads a record as RTMP's AVC sequence header and MP4's `avcC` box
    /// both carry it.
    pub fn parse(data: &Bytes) -> Result<Self, H264Error> {
        let mut reader = Reader::new(data);
        let version = reader.u8()?;
        if version != 1 {
            return Err(H264Error::UnsupportedConfigVersion(version));
        }
        // Three bytes repeating the first SPS's profile_idc, constraint
        // flags and level_idc. Skipped rather than stored: they are written
        // back out of the SPS itself, and two copies of one fact can
        // disagree.
        reader.skip(3)?;
        let nal_length_size = usize::from(reader.u8()? & 0x03) + 1;
        // The SPS count is five bits under three reserved ones; the PPS
        // count that follows the sets is a whole byte.
        let sps = reader.parameter_sets(0x1f)?;
        let pps = reader.parameter_sets(0xff)?;
        if sps.is_empty() {
            return Err(H264Error::MissingParameterSet("SPS"));
        }
        if pps.is_empty() {
            return Err(H264Error::MissingParameterSet("PPS"));
        }
        Ok(Self {
            parameters: Parameters { sps, pps },
            nal_length_size,
        })
    }

    /// Writes the record back out.
    ///
    /// The profile and level bytes come from the first SPS: a record whose
    /// header disagreed with the parameter set it carries would be
    /// describing two streams.
    pub fn to_bytes(&self) -> Result<Bytes, H264Error> {
        let Parameters { sps, pps } = &self.parameters;
        let mut out = BytesMut::new();
        out.put_u8(1);
        out.put_slice(self.parameters.profile_level()?);
        // The reserved bits of both counts are ones, and a decoder that
        // reads them as anything else will reject the record.
        out.put_u8(0xfc | (self.nal_length_size as u8 - 1));
        out.put_u8(0xe0 | (sps.len() as u8 & 0x1f));
        for set in sps {
            put_length_prefixed(&mut out, set.data(), 2)?;
        }
        out.put_u8(pps.len() as u8);
        for set in pps {
            put_length_prefixed(&mut out, set.data(), 2)?;
        }
        Ok(out.freeze())
    }
}

/// Bounds-checked forward reading, so a malformed record fails at the field
/// that is wrong rather than by panicking somewhere after it.
struct Reader<'a> {
    data: &'a Bytes,
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a Bytes) -> Self {
        Self { data, pos: 0 }
    }

    fn need(&self, count: usize) -> Result<(), H264Error> {
        if self.pos + count > self.data.len() {
            return Err(H264Error::Truncated {
                offset: self.pos,
                needed: self.pos + count - self.data.len(),
            });
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, H264Error> {
        self.need(1)?;
        self.pos += 1;
        Ok(self.data[self.pos - 1])
    }

    fn u16(&mut self) -> Result<u16, H264Error> {
        self.need(2)?;
        self.pos += 2;
        Ok(u16::from_be_bytes([
            self.data[self.pos - 2],
            self.data[self.pos - 1],
        ]))
    }

    fn skip(&mut self, count: usize) -> Result<(), H264Error> {
        self.need(count)?;
        self.pos += count;
        Ok(())
    }

    fn parameter_sets(&mut self, count_mask: u8) -> Result<Vec<Nal>, H264Error> {
        let count = self.u8()? & count_mask;
        let mut sets = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let length = usize::from(self.u16()?);
            if length == 0 {
                return Err(H264Error::EmptyNalUnit { offset: self.pos });
            }
            self.need(length)?;
            let set = Nal::new(self.data.slice(self.pos..self.pos + length))
                .ok_or(H264Error::EmptyNalUnit { offset: self.pos })?;
            sets.push(set);
            self.pos += length;
        }
        Ok(sets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(bytes: &'static [u8]) -> Nal {
        Nal::new(Bytes::from_static(bytes)).expect("not empty")
    }

    /// A syntactically plausible SPS: header byte, then the profile,
    /// constraint and level bytes an `AvcConfig` writes back out.
    fn sps() -> Nal {
        nal(&[0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0x80])
    }

    fn pps() -> Nal {
        nal(&[0x68, 0xce, 0x3c, 0x80])
    }

    fn idr() -> Nal {
        nal(&[0x65, 0x88, 0x84, 0x00])
    }

    #[test]
    fn a_unit_knows_what_it_is() {
        assert_eq!(sps().kind(), NalType::Sps);
        assert_eq!(pps().kind(), NalType::Pps);
        assert_eq!(idr().kind(), NalType::Idr);
        // The reference-idc bits above the type are not part of it.
        assert_eq!(nal(&[0x41, 0x9a]).kind(), NalType::Slice);
        assert_eq!(nal(&[0x1c, 0x00]).kind(), NalType::Other(28));
    }

    #[test]
    fn a_type_round_trips_through_its_number() {
        for byte in 0..=0x1fu8 {
            assert_eq!(NalType::from_header(byte).value(), byte);
        }
    }

    #[test]
    fn an_empty_unit_is_not_one() {
        assert_eq!(Nal::new(Bytes::new()), None);
    }

    #[test]
    fn splits_both_start_code_widths() {
        let mut stream = BytesMut::new();
        stream.put_slice(&[0, 0, 0, 1]);
        stream.put_slice(sps().data());
        stream.put_slice(&[0, 0, 1]);
        stream.put_slice(pps().data());
        stream.put_slice(&[0, 0, 0, 1]);
        stream.put_slice(idr().data());

        let nalus = split_annex_b(&stream.freeze());
        assert_eq!(nalus, vec![sps(), pps(), idr()]);
    }

    #[test]
    fn a_unit_ending_in_zeros_keeps_them() {
        // The last byte of `idr()` is zero, and the next start code begins
        // after it. Ending a unit at "three bytes before the next unit"
        // rather than at where the next start code begins would eat it.
        let mut stream = BytesMut::new();
        stream.put_slice(&[0, 0, 0, 1]);
        stream.put_slice(idr().data());
        stream.put_slice(&[0, 0, 0, 1]);
        stream.put_slice(pps().data());

        let nalus = split_annex_b(&stream.freeze());
        assert_eq!(nalus, vec![idr(), pps()]);
    }

    #[test]
    fn annex_b_round_trips() {
        let nalus = vec![sps(), pps(), idr()];
        assert_eq!(split_annex_b(&to_annex_b(&nalus)), nalus);
    }

    #[test]
    fn length_prefixed_round_trips_at_every_width() {
        let nalus = vec![sps(), pps(), idr()];
        for size in [1, 2, 4] {
            let framed = to_length_prefixed(&nalus, size).expect("units are short");
            assert_eq!(
                split_length_prefixed(&framed, size).unwrap(),
                nalus,
                "{size}"
            );
        }
    }

    #[test]
    fn a_truncated_length_prefixed_unit_is_refused() {
        let framed = to_length_prefixed(&[idr()], 4).unwrap();
        let short = framed.slice(..framed.len() - 1);
        assert_eq!(
            split_length_prefixed(&short, 4),
            Err(H264Error::Truncated {
                offset: 4,
                needed: 1
            })
        );
    }

    #[test]
    fn the_wrong_prefix_width_is_refused_rather_than_misread() {
        // Read as 2-byte prefixes, the 4-byte framing's leading zeros make a
        // zero-length unit — which is the shape of the failure, not a
        // plausible stream.
        let framed = to_length_prefixed(&[idr()], 4).unwrap();
        assert!(split_length_prefixed(&framed, 2).is_err());
    }

    #[test]
    fn a_unit_too_long_for_its_prefix_is_refused() {
        let long = Nal::new(Bytes::from(vec![0x65; 300])).unwrap();
        assert_eq!(
            to_length_prefixed(&[long], 1),
            Err(H264Error::NalUnitTooLong {
                length: 300,
                size: 1
            })
        );
    }

    fn parameters() -> Parameters {
        Parameters {
            sps: vec![sps()],
            pps: vec![pps()],
        }
    }

    fn config() -> AvcConfig {
        AvcConfig {
            parameters: parameters(),
            nal_length_size: 4,
        }
    }

    #[test]
    fn config_round_trips() {
        let parsed = AvcConfig::parse(&config().to_bytes().unwrap()).unwrap();
        assert_eq!(parsed, config());
    }

    #[test]
    fn config_takes_its_profile_from_the_sps() {
        let record = config().to_bytes().unwrap();
        assert_eq!(&record[1..4], &sps().data()[1..4]);
        assert_eq!(parameters().profile_level().unwrap(), &sps().data()[1..4]);
    }

    #[test]
    fn parameters_without_an_sps_describe_nothing() {
        let empty = Parameters {
            sps: Vec::new(),
            pps: vec![pps()],
        };
        assert_eq!(
            empty.profile_level(),
            Err(H264Error::MissingParameterSet("SPS"))
        );
    }

    #[test]
    fn a_config_without_a_pps_is_refused() {
        let mut record = BytesMut::new();
        record.put_slice(&[1, 0x42, 0xc0, 0x1e, 0xff, 0xe1]);
        record.put_u16(sps().data().len() as u16);
        record.put_slice(sps().data());
        record.put_u8(0);
        assert_eq!(
            AvcConfig::parse(&record.freeze()),
            Err(H264Error::MissingParameterSet("PPS"))
        );
    }

    #[test]
    fn a_config_of_another_version_is_refused() {
        let record = Bytes::from_static(&[2, 0x42, 0xc0, 0x1e, 0xff, 0xe1]);
        assert_eq!(
            AvcConfig::parse(&record),
            Err(H264Error::UnsupportedConfigVersion(2))
        );
    }

    #[test]
    fn a_keyframe_is_found_past_a_leading_sei() {
        let sei = nal(&[0x06, 0x05, 0x01, 0x80]);
        assert!(is_keyframe(&[sei.clone(), idr()]));
        assert!(!is_keyframe(&[sei, nal(&[0x41, 0x9a])]));
    }
}
