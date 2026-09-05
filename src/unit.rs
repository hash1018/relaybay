//! What every protocol in this crate agrees on.
//!
//! A [`Unit`] is one access unit — a coded picture, or a frame of sound —
//! with the track it belongs to and when it belongs there. That is all a
//! relay needs: an ingest makes them, a path fans them out, and each egress
//! packages them for its own protocol. What a track *is* lives beside them,
//! in a [`crate::track::Description`].
//!
//! # Why the two kinds are separate types
//!
//! A picture may be decoded out of order, so it has both a presentation and
//! a decode time, and it may or may not be a place a reader can start.
//! Sound is neither: AAC is never reordered and every frame stands alone.
//! Giving [`AudioUnit`] a decode time would mean a field that is always the
//! presentation time, and a keyframe flag that is always set — two fields
//! saying nothing, on the type that carries the most units.
//!
//! So each holds what it has, and [`Unit`] puts them in one queue. What a
//! path needs of a unit without caring which it is — which track, when, and
//! whether a reader may join at it — is on [`Unit`] itself.

use std::time::Duration;

use bytes::Bytes;

use crate::codec::h264;
use crate::track::TrackId;

/// One coded picture, under the codec that says how to read it.
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

    /// How many coded bytes this is, not counting whatever framing an egress
    /// will add. What a queue holding units has to measure itself in: their
    /// number says nothing, since one picture can outweigh a thousand frames
    /// of sound.
    pub fn len(&self) -> usize {
        match self {
            Self::H264(nalus) => nalus.iter().map(|nalu| nalu.data().len()).sum(),
        }
    }

    /// Whether it carries nothing, which a well-formed picture never does.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One frame of sound, under the codec that says how to read it.
///
/// There is no `is_keyframe` here to match [`VideoPayload`]'s: every frame
/// of every codec this carries stands alone, so the question has one answer
/// and asking it would suggest otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioPayload {
    /// AAC, as one raw access unit — no ADTS header, which is a framing that
    /// belongs to the protocols asking for one. See the crate docs.
    Aac(Bytes),
}

impl AudioPayload {
    /// How many coded bytes this is. See [`VideoPayload::len`].
    pub fn len(&self) -> usize {
        match self {
            Self::Aac(frame) => frame.len(),
        }
    }

    /// Whether it carries nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One coded picture, and where it belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoUnit {
    /// Which of the stream's tracks this is part of.
    pub track: TrackId,

    /// The coded picture.
    pub payload: VideoPayload,

    /// Presentation time, from the start of the stream.
    ///
    /// Relative rather than absolute because a reader that joins late still
    /// needs a timeline that starts at zero, and because the protocols
    /// disagree about the epoch: RTMP counts milliseconds from the first
    /// message, RTP counts clock ticks from a random offset. Both are
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
    pub fn new(track: TrackId, payload: VideoPayload, pts: Duration, dts: Duration) -> Self {
        Self {
            track,
            keyframe: payload.is_keyframe(),
            payload,
            pts,
            dts,
        }
    }
}

/// One frame of sound, and where it belongs.
///
/// Shorter than [`VideoUnit`] by exactly what sound does not have: no decode
/// time, because nothing reorders it, and no keyframe flag, because every
/// frame is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioUnit {
    /// Which of the stream's tracks this is part of.
    pub track: TrackId,

    /// The coded frame.
    pub payload: AudioPayload,

    /// Presentation time, from the start of the stream. See
    /// [`VideoUnit::pts`].
    pub pts: Duration,
}

/// One access unit of either kind.
///
/// What a path holds and fans out. One queue rather than two, so that
/// pictures and sound keep the order they arrived in: an egress sends them
/// in that order, and a queue trimmed back to the last keyframe drops the
/// sound between there and here along with it.
///
/// Cloning is cheap and is how a path serves more than one reader:
/// `bytes::Bytes` shares its buffer, so fan-out copies the list and not the
/// media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    Video(VideoUnit),
    Audio(AudioUnit),
}

impl Unit {
    /// Which of the stream's tracks this belongs to.
    pub fn track(&self) -> TrackId {
        match self {
            Self::Video(unit) => unit.track,
            Self::Audio(unit) => unit.track,
        }
    }

    /// When it belongs.
    pub fn pts(&self) -> Duration {
        match self {
            Self::Video(unit) => unit.pts,
            Self::Audio(unit) => unit.pts,
        }
    }

    /// Whether a reader given nothing before this could decode it, which is
    /// what a path asks when it is deciding where a new one may join and
    /// what a full queue may drop.
    ///
    /// Always true for sound. That is not a stand-in for an answer this
    /// cannot give: an AAC frame really can be decoded on its own, and a
    /// reader that started at one would hear it.
    pub fn is_keyframe(&self) -> bool {
        match self {
            Self::Video(unit) => unit.keyframe,
            Self::Audio(_) => true,
        }
    }

    /// Whether this is a picture a reader can be started at, which is not
    /// the same question as [`Unit::is_keyframe`].
    ///
    /// A queue holding a stream back to the last place a reader could join
    /// has to cut at a picture. Cutting at sound would leave the pictures
    /// before it, which nothing can decode.
    pub fn opens_a_stream(&self) -> bool {
        matches!(self, Self::Video(unit) if unit.keyframe)
    }

    /// How many coded bytes this carries.
    pub fn len(&self) -> usize {
        match self {
            Self::Video(unit) => unit.payload.len(),
            Self::Audio(unit) => unit.payload.len(),
        }
    }

    /// Whether it carries nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<VideoUnit> for Unit {
    fn from(unit: VideoUnit) -> Self {
        Self::Video(unit)
    }
}

impl From<AudioUnit> for Unit {
    fn from(unit: AudioUnit) -> Self {
        Self::Audio(unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::aac;
    use crate::track::{Codec, Description};

    fn description() -> Description {
        let sps = h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e])).unwrap();
        let pps = h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80])).unwrap();
        Description::new(vec![
            Codec::H264(h264::Parameters {
                sps: vec![sps],
                pps: vec![pps],
            }),
            Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()),
        ])
        .unwrap()
    }

    fn nal(bytes: &'static [u8]) -> h264::Nal {
        h264::Nal::new(Bytes::from_static(bytes)).expect("not empty")
    }

    #[test]
    fn a_unit_reads_its_own_keyframe_flag() {
        let video = description().tracks()[0].id();
        let idr = VideoPayload::H264(vec![nal(&[0x65, 0x88])]);
        let inter = VideoPayload::H264(vec![nal(&[0x41, 0x9a])]);
        assert!(VideoUnit::new(video, idr, Duration::ZERO, Duration::ZERO).keyframe);
        assert!(!VideoUnit::new(video, inter, Duration::ZERO, Duration::ZERO).keyframe);
    }

    #[test]
    fn a_path_can_ask_a_unit_what_it_needs_without_knowing_which_kind_it_is() {
        let description = description();
        let video = description.tracks()[0].id();
        let audio = description.tracks()[1].id();

        let picture = Unit::from(VideoUnit::new(
            video,
            VideoPayload::H264(vec![nal(&[0x41, 0x9a])]),
            Duration::from_millis(40),
            Duration::from_millis(40),
        ));
        let sound = Unit::from(AudioUnit {
            track: audio,
            payload: AudioPayload::Aac(Bytes::from_static(&[0x21, 0x00])),
            pts: Duration::from_millis(23),
        });

        assert_eq!(picture.track(), video);
        assert_eq!(picture.pts(), Duration::from_millis(40));
        assert!(!picture.is_keyframe());

        assert_eq!(sound.track(), audio);
        assert_eq!(sound.pts(), Duration::from_millis(23));
        assert!(sound.is_keyframe(), "every frame of sound stands alone");
    }

    #[test]
    fn a_unit_names_a_track_that_exists() {
        let description = description();
        let audio = description.tracks()[1].id();
        let sound = Unit::from(AudioUnit {
            track: audio,
            payload: AudioPayload::Aac(Bytes::new()),
            pts: Duration::ZERO,
        });
        let track = description
            .track(sound.track())
            .expect("in this description");
        assert!(matches!(track.codec(), Codec::Aac(_)));
    }
}
