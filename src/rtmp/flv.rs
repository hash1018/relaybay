//! What is in front of the coded bytes in an audio or video message.
//!
//! RTMP does not put media straight into a message. It puts an FLV tag body
//! there — the same few bytes a `.flv` file uses, which is where the name
//! comes from — and the media follows that. The tag says which codec, which
//! kind of frame, and whether these bytes are the stream's parameters or a
//! frame of it.
//!
//! ```text
//! video: [frame type | codec][packet type][composition time :24][payload]
//! audio: [format | rate | size | type][packet type][payload]
//! ```
//!
//! # Where a description comes from
//!
//! Packet type 0 is not media. It is the record a decoder is configured
//! with — an `AVCDecoderConfigurationRecord` for video, an
//! `AudioSpecificConfig` for audio — sent once, before anything else on that
//! track. That is the message a [`crate::track::Description`] is built from,
//! and until it has arrived a publisher has sent nothing that can be served.
//!
//! # The signed field
//!
//! A video tag's composition time is how far a picture's presentation time
//! is from its decode time, and it is a **signed** 24-bit number. A stream
//! with B-frames sends negative ones. Read as unsigned, `-1` becomes
//! 16 777 215 — a presentation time four and a half hours into the future,
//! on a picture that should have been shown a millisecond ago.

use bytes::{BufMut, Bytes, BytesMut};

/// The codec ids this reads. Everything else is refused by name.
mod codec {
    /// H.264, and the only video codec a legacy tag carries here.
    pub const AVC: u8 = 7;
    /// AAC, and the only audio codec a legacy tag carries here.
    pub const AAC: u8 = 10;
}

/// What can be wrong with a tag.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlvError {
    /// The tag ended before its header did.
    #[error("truncated at byte {offset}: {needed} more byte(s) needed")]
    Truncated { offset: usize, needed: usize },

    /// A video codec this does not read. Named rather than passed through:
    /// the bytes after the first mean different things per codec, so an
    /// unrecognized one is a payload of unknown shape.
    #[error("video codec id {0} is one this does not read")]
    UnreadVideoCodec(u8),

    /// An audio codec this does not read.
    #[error("audio format id {0} is one this does not read")]
    UnreadAudioFormat(u8),

    /// The extended tag header that enhanced RTMP uses to carry HEVC, AV1
    /// and VP9. A different header shape, not a codec id, which is why it is
    /// its own error.
    #[error("an enhanced RTMP tag for {codec}, which this does not read")]
    Enhanced { codec: String },

    /// A packet type outside the three a tag defines.
    #[error("packet type {0} is not one of the ones defined")]
    UnknownPacketType(u8),

    /// A composition time too large for the three bytes that carry it.
    #[error("a composition time of {0} ms does not fit a signed 24-bit field")]
    CompositionTimeTooLarge(i32),
}

/// What a video message carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoTag {
    /// The `AVCDecoderConfigurationRecord` a decoder is set up from, which
    /// arrives once before any picture. See [`crate::codec::h264::AvcConfig`].
    SequenceHeader(Bytes),

    /// One access unit, still length-prefixed at the width the sequence
    /// header declared.
    Picture {
        /// What the publisher said this is. The answer a unit carries is
        /// read from the NAL units themselves instead — see
        /// [`crate::unit::VideoPayload::is_keyframe`] — because that is what
        /// a decoder will do, and encoders that set this wrong exist. This
        /// is here because a tag going back out has to state something.
        keyframe: bool,

        /// How far this picture's presentation time is from its decode
        /// time, in milliseconds. Zero without B-frames, negative with them.
        composition_time: i32,

        /// The access unit, length-prefixed.
        data: Bytes,
    },

    /// The end of the sequence: no more pictures on this track. Carries
    /// nothing.
    End,
}

/// What an audio message carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioTag {
    /// The `AudioSpecificConfig` a decoder is set up from, which arrives
    /// once before any frame. See [`crate::codec::aac::Parameters`].
    SequenceHeader(Bytes),

    /// One raw access unit, with no ADTS header.
    Frame(Bytes),
}

/// Reads a video message's payload.
pub fn read_video(data: &Bytes) -> Result<VideoTag, FlvError> {
    let first = *byte(data, 0)?;

    // Enhanced RTMP moved the codec out of four bits and into a FourCC, and
    // says so with the top bit. Nothing here reads that shape, but the
    // FourCC is worth digging out so a log says which codec was refused.
    if first & 0x80 != 0 {
        return Err(FlvError::Enhanced {
            codec: fourcc(data.get(1..5)),
        });
    }

    let codec = first & 0x0f;
    if codec != codec::AVC {
        return Err(FlvError::UnreadVideoCodec(codec));
    }
    // Frame type 1 is a keyframe and 4 is a keyframe a server generated;
    // 2 and 3 are inter pictures, and 5 is a command rather than a picture.
    let keyframe = matches!(first >> 4, 1 | 4);

    match byte(data, 1)? {
        0 => Ok(VideoTag::SequenceHeader(data.slice(5..))),
        1 => Ok(VideoTag::Picture {
            keyframe,
            composition_time: si24(data.get(2..5).ok_or_else(|| FlvError::Truncated {
                offset: data.len(),
                needed: 5 - data.len(),
            })?),
            data: data.slice(5..),
        }),
        2 => Ok(VideoTag::End),
        other => Err(FlvError::UnknownPacketType(*other)),
    }
}

/// Reads an audio message's payload.
///
/// The sample rate, sample size and channel count in the first byte are read
/// past rather than read. For AAC they are required to say 44 kHz, 16-bit
/// and stereo whatever the stream actually is, and the true values are in
/// the `AudioSpecificConfig` — a 48 kHz mono stream still has 44 kHz stereo
/// in this byte. Believing it is a standing bug in FLV readers.
pub fn read_audio(data: &Bytes) -> Result<AudioTag, FlvError> {
    let first = *byte(data, 0)?;
    let format = first >> 4;
    if format != codec::AAC {
        return Err(FlvError::UnreadAudioFormat(format));
    }
    match byte(data, 1)? {
        0 => Ok(AudioTag::SequenceHeader(data.slice(2..))),
        1 => Ok(AudioTag::Frame(data.slice(2..))),
        other => Err(FlvError::UnknownPacketType(*other)),
    }
}

/// Writes a video message's payload.
pub fn write_video(tag: &VideoTag) -> Result<Bytes, FlvError> {
    let mut out = BytesMut::new();
    match tag {
        VideoTag::SequenceHeader(record) => {
            out.put_u8(0x10 | codec::AVC);
            out.put_u8(0);
            put_si24(&mut out, 0)?;
            out.put_slice(record);
        }
        VideoTag::Picture {
            keyframe,
            composition_time,
            data,
        } => {
            out.put_u8(if *keyframe { 0x10 } else { 0x20 } | codec::AVC);
            out.put_u8(1);
            put_si24(&mut out, *composition_time)?;
            out.put_slice(data);
        }
        VideoTag::End => {
            out.put_u8(0x10 | codec::AVC);
            out.put_u8(2);
            put_si24(&mut out, 0)?;
        }
    }
    Ok(out.freeze())
}

/// Writes an audio message's payload.
///
/// The three fields after the format are written as the specification
/// requires for AAC — 44 kHz, 16-bit, stereo — whatever the stream is. See
/// [`read_audio`].
pub fn write_audio(tag: &AudioTag) -> Bytes {
    let mut out = BytesMut::new();
    out.put_u8((codec::AAC << 4) | 0x0f);
    match tag {
        AudioTag::SequenceHeader(record) => {
            out.put_u8(0);
            out.put_slice(record);
        }
        AudioTag::Frame(frame) => {
            out.put_u8(1);
            out.put_slice(frame);
        }
    }
    out.freeze()
}

fn byte(data: &Bytes, index: usize) -> Result<&u8, FlvError> {
    // `ok_or_else` rather than `ok_or`: the subtraction is only meaningful
    // on the branch where the byte is missing, and would underflow on the
    // one where it is not.
    data.get(index).ok_or_else(|| FlvError::Truncated {
        offset: data.len(),
        needed: index + 1 - data.len(),
    })
}

/// A FourCC as something a person can read, for an error message.
fn fourcc(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(bytes) if bytes.iter().all(u8::is_ascii_graphic) => {
            String::from_utf8_lossy(bytes).into_owned()
        }
        Some(bytes) => format!("{bytes:02x?}"),
        None => "an unstated codec".to_owned(),
    }
}

/// Reads three bytes as a signed number. See the module docs on why this is
/// not a `u24`.
fn si24(bytes: &[u8]) -> i32 {
    let value = i32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
    if value & 0x80_0000 == 0 {
        value
    } else {
        value - 0x100_0000
    }
}

fn put_si24(out: &mut BytesMut, value: i32) -> Result<(), FlvError> {
    if !(-0x80_0000..0x80_0000).contains(&value) {
        return Err(FlvError::CompositionTimeTooLarge(value));
    }
    let value = if value < 0 { value + 0x100_0000 } else { value };
    out.put_slice(&value.to_be_bytes()[1..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(bytes: &'static [u8]) -> Bytes {
        Bytes::from_static(bytes)
    }

    #[test]
    fn a_sequence_header_is_the_record_after_five_bytes() {
        // Keyframe, AVC, packet type 0, no composition time, then the
        // record. The frame type on a sequence header is 1 by convention
        // and means nothing.
        let tag = read_video(&video(&[0x17, 0x00, 0, 0, 0, 0x01, 0x42, 0xc0])).unwrap();
        assert_eq!(
            tag,
            VideoTag::SequenceHeader(Bytes::from_static(&[0x01, 0x42, 0xc0]))
        );
    }

    #[test]
    fn a_keyframe_and_an_inter_picture_are_told_apart() {
        let key = read_video(&video(&[0x17, 0x01, 0, 0, 0, 0x65])).unwrap();
        let inter = read_video(&video(&[0x27, 0x01, 0, 0, 0, 0x41])).unwrap();
        assert!(matches!(key, VideoTag::Picture { keyframe: true, .. }));
        assert!(matches!(
            inter,
            VideoTag::Picture {
                keyframe: false,
                ..
            }
        ));

        // Frame type 4 is a keyframe a server generated, and is still a
        // place a reader can start.
        let generated = read_video(&video(&[0x47, 0x01, 0, 0, 0, 0x65])).unwrap();
        assert!(matches!(
            generated,
            VideoTag::Picture { keyframe: true, .. }
        ));
    }

    #[test]
    fn a_negative_composition_time_stays_negative() {
        // 0xffffff is -1, not 16 777 215. See the module docs.
        let tag = read_video(&video(&[0x27, 0x01, 0xff, 0xff, 0xff, 0x41])).unwrap();
        let VideoTag::Picture {
            composition_time, ..
        } = tag
        else {
            panic!("a picture")
        };
        assert_eq!(composition_time, -1);
    }

    #[test]
    fn every_composition_time_the_field_holds_round_trips() {
        for value in [-0x80_0000, -1000, -1, 0, 1, 1000, 0x7f_ffff] {
            let tag = VideoTag::Picture {
                keyframe: false,
                composition_time: value,
                data: Bytes::from_static(&[0x41]),
            };
            let written = write_video(&tag).unwrap();
            assert_eq!(read_video(&written).unwrap(), tag, "{value}");
        }
    }

    #[test]
    fn a_composition_time_the_field_cannot_hold_is_refused() {
        let tag = VideoTag::Picture {
            keyframe: false,
            composition_time: 0x80_0000,
            data: Bytes::new(),
        };
        assert_eq!(
            write_video(&tag),
            Err(FlvError::CompositionTimeTooLarge(0x80_0000))
        );
    }

    #[test]
    fn a_video_tag_round_trips() {
        for tag in [
            VideoTag::SequenceHeader(Bytes::from_static(&[0x01, 0x42, 0xc0, 0x1e])),
            VideoTag::Picture {
                keyframe: true,
                composition_time: 33,
                data: Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]),
            },
            VideoTag::End,
        ] {
            let written = write_video(&tag).unwrap();
            assert_eq!(read_video(&written).unwrap(), tag);
        }
    }

    #[test]
    fn an_audio_tag_round_trips() {
        for tag in [
            AudioTag::SequenceHeader(Bytes::from_static(&[0x12, 0x10])),
            AudioTag::Frame(Bytes::from_static(&[0x21, 0x00, 0x03])),
        ] {
            assert_eq!(read_audio(&write_audio(&tag)).unwrap(), tag);
        }
    }

    #[test]
    fn an_audio_tag_says_what_it_carries_and_nothing_about_its_rate() {
        // A 48 kHz mono stream, whose first byte still claims 44 kHz stereo
        // because the specification says it must. See `read_audio`.
        let tag = read_audio(&video(&[0xaf, 0x01, 0x21, 0x00])).unwrap();
        assert_eq!(tag, AudioTag::Frame(Bytes::from_static(&[0x21, 0x00])));
    }

    #[test]
    fn a_codec_this_does_not_read_is_named() {
        // Codec 2 is Sorenson H.263, which RTMP could carry and nothing
        // sends any more.
        assert_eq!(
            read_video(&video(&[0x12, 0x00])),
            Err(FlvError::UnreadVideoCodec(2))
        );
        // Format 2 is MP3.
        assert_eq!(
            read_audio(&video(&[0x2f, 0x00])),
            Err(FlvError::UnreadAudioFormat(2))
        );
    }

    #[test]
    fn an_enhanced_tag_says_which_codec_it_wanted() {
        let mut tag = BytesMut::from(&[0x91u8][..]);
        tag.put_slice(b"hvc1");
        assert_eq!(
            read_video(&tag.freeze()),
            Err(FlvError::Enhanced {
                codec: "hvc1".to_owned()
            })
        );
    }

    #[test]
    fn a_packet_type_that_is_not_defined_is_refused() {
        assert_eq!(
            read_video(&video(&[0x17, 0x09])),
            Err(FlvError::UnknownPacketType(9))
        );
        assert_eq!(
            read_audio(&video(&[0xaf, 0x09])),
            Err(FlvError::UnknownPacketType(9))
        );
    }

    #[test]
    fn a_tag_shorter_than_its_own_header_is_refused() {
        assert_eq!(
            read_video(&Bytes::new()),
            Err(FlvError::Truncated {
                offset: 0,
                needed: 1
            })
        );
        assert_eq!(
            read_video(&video(&[0x17])),
            Err(FlvError::Truncated {
                offset: 1,
                needed: 1
            })
        );
        // A picture needs the three composition time bytes as well.
        assert_eq!(
            read_video(&video(&[0x17, 0x01, 0, 0])),
            Err(FlvError::Truncated {
                offset: 4,
                needed: 1
            })
        );
        assert_eq!(
            read_audio(&video(&[0xaf])),
            Err(FlvError::Truncated {
                offset: 1,
                needed: 1
            })
        );
    }

    #[test]
    fn a_picture_with_no_payload_is_a_picture() {
        // An empty access unit is odd but not malformed, and refusing it
        // here would drop a whole publish over one message.
        assert_eq!(
            read_video(&video(&[0x17, 0x01, 0, 0, 0])).unwrap(),
            VideoTag::Picture {
                keyframe: true,
                composition_time: 0,
                data: Bytes::new()
            }
        );
    }
}
