//! What every protocol in this crate agrees on.

use std::time::Duration;

use crate::codec::h264;

/// One access unit's coded picture, under the codec that says how to read
/// it.
///
/// The codec belongs here rather than on each NAL unit because it is a
/// property of the stream: every unit of one picture is the same codec, and
/// tagging them individually would let a picture be assembled out of two.
///
/// It is an enum rather than a byte payload plus a codec name so that
/// nothing can read HEVC unit types as H.264 ones. The five bits that mean
/// "IDR slice" in H.264 are part of a six-bit field at another offset in
/// HEVC, naming something else entirely, and no amount of care at the call
/// site catches that — only the compiler does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoPayload {
    /// H.264, as a list of unframed NAL units in bitstream order.
    H264(Vec<h264::Nal>),
}

impl VideoPayload {
    /// Whether this picture can be decoded with nothing before it, which is
    /// what makes it a place a new reader can be started from.
    pub fn is_keyframe(&self) -> bool {
        match self {
            Self::H264(nalus) => h264::is_keyframe(nalus),
        }
    }

    /// Whether the picture carries the parameter sets a decoder needs, so
    /// that an egress which cannot state them separately has nothing to put
    /// in front of it.
    pub fn carries_parameter_sets(&self) -> bool {
        match self {
            Self::H264(nalus) => h264::carries_parameter_sets(nalus),
        }
    }
}

/// One access unit: the coded picture, and when it belongs.
///
/// The NAL units inside carry no framing — neither Annex-B start codes nor
/// length prefixes — because both are framings *of* a unit list, chosen by
/// whichever protocol is carrying it. See the crate docs.
///
/// Cloning is cheap and is how a path serves more than one reader:
/// `bytes::Bytes` shares its buffer, so fan-out copies the list and not the
/// pictures.
#[derive(Clone, Debug)]
pub struct VideoUnit {
    /// The coded picture.
    pub payload: VideoPayload,

    /// Presentation time, from the start of the stream.
    ///
    /// Relative rather than absolute because a reader that joins late still
    /// needs a timeline that starts at zero, and because the protocols
    /// disagree about the epoch: RTMP counts milliseconds from the first
    /// message, RTP counts 90 kHz ticks from a random offset. Both are
    /// derived at egress; neither is stored.
    pub pts: Duration,

    /// Decode time, which is `pts` for a stream without B-frames and earlier
    /// than it for one with them. Never later.
    pub dts: Duration,

    /// Whether this unit can be decoded with nothing before it.
    ///
    /// Cached rather than read from `payload` each time: every egress asks —
    /// it decides where a new reader may join, and which pictures a full
    /// queue may drop — and the answer cannot change once the unit is made.
    pub keyframe: bool,
}

impl VideoUnit {
    /// Builds a unit, reading from the payload whether it is a keyframe.
    pub fn new(payload: VideoPayload, pts: Duration, dts: Duration) -> Self {
        Self {
            keyframe: payload.is_keyframe(),
            payload,
            pts,
            dts,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn nal(bytes: &'static [u8]) -> h264::Nal {
        h264::Nal::new(Bytes::from_static(bytes)).expect("not empty")
    }

    #[test]
    fn a_unit_reads_its_own_keyframe_flag() {
        let idr = VideoPayload::H264(vec![nal(&[0x65, 0x88])]);
        let inter = VideoPayload::H264(vec![nal(&[0x41, 0x9a])]);
        assert!(VideoUnit::new(idr, Duration::ZERO, Duration::ZERO).keyframe);
        assert!(!VideoUnit::new(inter, Duration::ZERO, Duration::ZERO).keyframe);
    }
}
