//! What a stream is made of, as opposed to what is going through it.
//!
//! A path carries two things: a description, which says what the tracks are
//! and what a decoder needs to start on each, and a run of
//! [`crate::unit::Unit`]s, which is the coded media itself. The split is
//! forced by the protocols — every one of them states the first before it
//! sends any of the second, because a reader that joined without it would
//! have bytes it could not decode.
//!
//! | egress | states the description as             |
//! | ------ | ------------------------------------- |
//! | RTSP   | SDP, once, before `PLAY`              |
//! | HLS    | an `init.mp4`, and the playlist's `CODECS` |
//! | WebRTC | the SDP answer, and in front of every keyframe |
//! | RTMP   | an AVC or AAC sequence header         |
//!
//! All four are the same facts in four notations, so this holds the facts
//! and each egress writes its own notation. Nothing here knows what SDP
//! looks like.
//!
//! # A description does not change
//!
//! It is fixed for the life of a stream. A publisher that changes what it is
//! sending — a new resolution, so a new SPS — is starting a different
//! stream, and its readers are torn down and rebuilt against the new
//! description rather than being handed one mid-flight. That is what lets a
//! [`TrackId`] be an index: nothing renumbers underneath it.

use crate::codec::{aac, h264};

/// Which of a stream's tracks a unit belongs to.
///
/// An index into the description that produced it, and meaningless against
/// any other. There is no public way to make one, so a unit's track always
/// names a track that exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u16);

impl TrackId {
    /// Where the track sits in its description.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// Whether a track is pictures or sound.
///
/// Read from the codec rather than stated beside it: H.264 is video, and a
/// second copy of that fact could only ever disagree with the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Video,
    Audio,
}

/// A track's codec, and what a decoder has to be given to start on it.
///
/// The two are one type because neither is any use without the other: a
/// track that said "H.264" without parameter sets would describe something
/// no reader could play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    H264(h264::Parameters),
    Aac(aac::Parameters),
}

impl Codec {
    /// Whether this is pictures or sound.
    pub fn kind(&self) -> Kind {
        match self {
            Self::H264(_) => Kind::Video,
            Self::Aac(_) => Kind::Audio,
        }
    }

    /// The rate a timestamp on this track is counted in, which RTP calls the
    /// clock rate.
    ///
    /// Fixed at 90 kHz for H.264 by the RFC that packetizes it, whatever the
    /// frame rate; for AAC it is the sample rate, because a timestamp counts
    /// samples.
    pub fn clock_rate(&self) -> u32 {
        match self {
            Self::H264(_) => 90_000,
            Self::Aac(parameters) => parameters.sample_rate(),
        }
    }
}

/// One track: which it is, and what it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    id: TrackId,
    codec: Codec,
}

impl Track {
    /// Which track this is, as its units name it.
    pub fn id(&self) -> TrackId {
        self.id
    }

    /// What it carries.
    pub fn codec(&self) -> &Codec {
        &self.codec
    }

    /// Whether it is pictures or sound.
    pub fn kind(&self) -> Kind {
        self.codec.kind()
    }
}

/// Why a set of tracks does not describe a stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DescriptionError {
    /// A stream with no tracks is not a stream.
    #[error("a description with no tracks describes nothing")]
    NoTracks,

    /// More tracks than a [`TrackId`] can name. Nothing sane comes near it;
    /// the check is here so that the cast to an index cannot be wrong.
    #[error("more than {limit} tracks")]
    TooManyTracks { limit: usize },

    /// An H.264 track without the parameter sets a decoder starts from. It
    /// would be published, and every reader would get bytes it could not
    /// begin on.
    #[error("track {index} is H.264 with no {missing}, which no decoder can start from")]
    MissingParameterSet { index: usize, missing: &'static str },
}

/// What a stream is: its tracks, in the order they were declared.
///
/// Built once, checked once, and then immutable — see the module docs on why
/// it has to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    tracks: Vec<Track>,
}

impl Description {
    /// Numbers the codecs into tracks, refusing a set that could not be
    /// played.
    ///
    /// The checks are here rather than at each egress because there are
    /// going to be several egresses and one ingest: a stream that cannot be
    /// described should fail where it is published, with the publisher still
    /// connected to be told, and not once per reader.
    pub fn new(codecs: Vec<Codec>) -> Result<Self, DescriptionError> {
        if codecs.is_empty() {
            return Err(DescriptionError::NoTracks);
        }
        let limit = usize::from(u16::MAX);
        if codecs.len() > limit {
            return Err(DescriptionError::TooManyTracks { limit });
        }
        for (index, codec) in codecs.iter().enumerate() {
            if let Codec::H264(parameters) = codec {
                let missing = if parameters.sps.is_empty() {
                    "SPS"
                } else if parameters.pps.is_empty() {
                    "PPS"
                } else {
                    continue;
                };
                return Err(DescriptionError::MissingParameterSet { index, missing });
            }
        }
        Ok(Self {
            tracks: codecs
                .into_iter()
                .enumerate()
                .map(|(index, codec)| Track {
                    id: TrackId(index as u16),
                    codec,
                })
                .collect(),
        })
    }

    /// Every track, in order.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The track a unit belongs to.
    ///
    /// `None` only for an id from some other description, which is the one
    /// way a unit and a description can fail to match.
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.get(id.index())
    }

    /// The tracks that are pictures, or the ones that are sound.
    ///
    /// An egress that can carry one video track and one audio track — which
    /// is most of them — takes the first of each and leaves the rest.
    pub fn of_kind(&self, kind: Kind) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |track| track.kind() == kind)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn h264() -> Codec {
        Codec::H264(h264::Parameters {
            sps: vec![h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e])).unwrap()],
            pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80])).unwrap()],
        })
    }

    fn aac() -> Codec {
        Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap())
    }

    #[test]
    fn tracks_are_numbered_by_where_they_sit() {
        let description = Description::new(vec![h264(), aac()]).unwrap();
        let ids: Vec<_> = description
            .tracks()
            .iter()
            .map(|track| track.id().index())
            .collect();
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn a_unit_finds_its_own_track() {
        let description = Description::new(vec![h264(), aac()]).unwrap();
        let audio = description.tracks()[1].id();
        assert_eq!(description.track(audio).unwrap().kind(), Kind::Audio);
    }

    #[test]
    fn an_id_from_another_description_finds_nothing() {
        let two = Description::new(vec![h264(), aac()]).unwrap();
        let one = Description::new(vec![h264()]).unwrap();
        assert_eq!(one.track(two.tracks()[1].id()), None);
    }

    #[test]
    fn what_a_track_is_comes_from_its_codec() {
        assert_eq!(h264().kind(), Kind::Video);
        assert_eq!(aac().kind(), Kind::Audio);
    }

    #[test]
    fn a_clock_rate_is_fixed_for_pictures_and_the_sample_rate_for_sound() {
        assert_eq!(h264().clock_rate(), 90_000);
        assert_eq!(aac().clock_rate(), 44100);
    }

    #[test]
    fn an_egress_can_ask_for_one_kind() {
        let description = Description::new(vec![aac(), h264(), aac()]).unwrap();
        let audio: Vec<_> = description
            .of_kind(Kind::Audio)
            .map(|track| track.id().index())
            .collect();
        assert_eq!(audio, [0, 2]);
        assert_eq!(description.of_kind(Kind::Video).count(), 1);
    }

    #[test]
    fn a_stream_with_no_tracks_is_refused() {
        assert_eq!(
            Description::new(Vec::new()),
            Err(DescriptionError::NoTracks)
        );
    }

    #[test]
    fn a_video_track_a_reader_could_not_start_on_is_refused() {
        let no_pps = Codec::H264(h264::Parameters {
            sps: vec![h264::Nal::new(Bytes::from_static(&[0x67, 0x42])).unwrap()],
            pps: Vec::new(),
        });
        assert_eq!(
            Description::new(vec![aac(), no_pps]),
            Err(DescriptionError::MissingParameterSet {
                index: 1,
                missing: "PPS"
            })
        );

        let no_sps = Codec::H264(h264::Parameters {
            sps: Vec::new(),
            pps: Vec::new(),
        });
        assert_eq!(
            Description::new(vec![no_sps]),
            Err(DescriptionError::MissingParameterSet {
                index: 0,
                missing: "SPS"
            })
        );
    }
}
