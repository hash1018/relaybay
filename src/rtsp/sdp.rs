//! Writing a [`Description`] as the text RTSP answers `DESCRIBE` with.
//!
//! SDP is a list of one-letter fields, and what a reader takes from it is
//! the same set of facts a track description already holds — what the
//! codecs are, what a decoder has to be given, and which clock the
//! timestamps count in. So this is a notation and nothing more; no decision
//! is made here that is not already in the description.
//!
//! ```text
//! v=0
//! o=- 0 0 IN IP4 0.0.0.0
//! s=relaybay
//! t=0 0
//! a=control:*
//! m=video 0 RTP/AVP 96
//! a=rtpmap:96 H264/90000
//! a=fmtp:96 packetization-mode=1; profile-level-id=42c01e; sprop-parameter-sets=Z0LAHtkA,aM48gA==
//! a=control:trackID=0
//! ```
//!
//! # Where the parameter sets go
//!
//! In the `fmtp` line, base64-encoded, rather than in the stream. A reader
//! that has the SDP can start on the first picture it is sent; one that had
//! to wait for the parameter sets in-band would wait for the publisher to
//! repeat them, which many never do.
//!
//! # The addresses are zero
//!
//! `c=` and the port in each `m=` line say nothing on purpose. Where the
//! packets go is settled per track by `SETUP`, which the client sends after
//! reading this — either a pair of its own UDP ports or a pair of channels
//! on the connection the SDP arrived on. An address here would be a second
//! answer to a question already asked elsewhere.

use crate::codec::{aac, h264};
use crate::track::{Codec, Description};

/// Writes the description as SDP.
///
/// `name` is what a player shows for the stream; the path it was asked for
/// is the natural thing to pass.
pub fn describe(description: &Description, name: &str) -> String {
    let mut out = String::new();
    // Version 0, which is the only one. The origin's fields are a username,
    // a session id, a version and an address, none of which mean anything
    // for a live stream that is not being negotiated.
    out.push_str("v=0\r\n");
    out.push_str("o=- 0 0 IN IP4 0.0.0.0\r\n");
    out.push_str(&format!("s={}\r\n", sanitize(name)));
    out.push_str("c=IN IP4 0.0.0.0\r\n");
    // A live stream has no beginning and no end to seek within.
    out.push_str("t=0 0\r\n");
    out.push_str("a=control:*\r\n");

    for track in description.tracks() {
        let id = track.id().index();
        match track.codec() {
            Codec::H264(parameters) => video(&mut out, parameters),
            Codec::Aac(parameters) => audio(&mut out, parameters),
        }
        // Relative to the `Content-Base` the response states, which is what
        // a client appends to build the URI it sends `SETUP` to.
        out.push_str(&format!("a=control:trackID={id}\r\n"));
    }
    out
}

fn video(out: &mut String, parameters: &h264::Parameters) {
    let payload_type = crate::rtp::payload_type::VIDEO;
    // Port 0: see the module docs on why there is no address here.
    out.push_str(&format!("m=video 0 RTP/AVP {payload_type}\r\n"));
    out.push_str(&format!("a=rtpmap:{payload_type} H264/90000\r\n"));

    let mut fmtp = format!("a=fmtp:{payload_type} packetization-mode=1");
    // The three bytes a decoder checks before it agrees to try. Absent from
    // a description whose SPS is too short to hold them, which is a stream
    // nothing could play anyway — the line is still worth sending without
    // it, because the parameter sets after it are what actually configure.
    if let Ok(profile) = parameters.profile_level() {
        fmtp.push_str(&format!("; profile-level-id={}", hex(profile)));
    }
    let sets: Vec<_> = parameters
        .sps
        .iter()
        .chain(&parameters.pps)
        .map(|nalu| base64(nalu.data()))
        .collect();
    if !sets.is_empty() {
        fmtp.push_str(&format!("; sprop-parameter-sets={}", sets.join(",")));
    }
    out.push_str(&fmtp);
    out.push_str("\r\n");
}

fn audio(out: &mut String, parameters: &aac::Parameters) {
    let payload_type = crate::rtp::payload_type::AUDIO;
    let rate = parameters.sample_rate();
    let channels = parameters.channels();
    out.push_str(&format!("m=audio 0 RTP/AVP {payload_type}\r\n"));
    out.push_str(&format!(
        "a=rtpmap:{payload_type} mpeg4-generic/{rate}/{channels}\r\n"
    ));
    // The three widths are not fixed by the specification, which is why they
    // are stated: a receiver reads the AU headers with whatever this says.
    // See [`crate::rtp::aac`].
    out.push_str(&format!(
        "a=fmtp:{payload_type} profile-level-id=1; mode=AAC-hbr; \
         sizelength=13; indexlength=3; indexdeltalength=3; config={}\r\n",
        hex(parameters.config())
    ));
}

/// Keeps a name to one line. A newline in the middle of an `s=` field would
/// end the field and start whatever came after it as a new one.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Base64, which is how SDP carries bytes.
///
/// Written out rather than depended on: it is the only place in the crate
/// that needs it, and a dependency an application has to compile is a worse
/// trade than twelve lines.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let mut packed = 0u32;
        for (index, byte) in group.iter().enumerate() {
            packed |= u32::from(*byte) << (16 - index * 8);
        }
        // One character per six bits, and a pad for every byte the last
        // group was short.
        for index in 0..4 {
            if index <= group.len() {
                out.push(ALPHABET[(packed >> (18 - index * 6)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn sps() -> h264::Nal {
        h264::Nal::new(Bytes::from_static(&[
            0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0x80,
        ]))
        .unwrap()
    }

    fn pps() -> h264::Nal {
        h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80])).unwrap()
    }

    fn both() -> Description {
        Description::new(vec![
            Codec::H264(h264::Parameters {
                sps: vec![sps()],
                pps: vec![pps()],
            }),
            Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()),
        ])
        .unwrap()
    }

    /// The value of the first attribute with this name.
    fn attribute<'a>(sdp: &'a str, name: &str) -> Option<&'a str> {
        sdp.lines()
            .find_map(|line| line.strip_prefix(&format!("a={name}:")))
    }

    #[test]
    fn a_description_becomes_the_lines_a_player_reads() {
        let sdp = describe(&both(), "live/cam1");
        let lines: Vec<_> = sdp.lines().collect();

        assert_eq!(lines[0], "v=0");
        assert_eq!(lines[2], "s=live/cam1");
        assert_eq!(lines[4], "t=0 0");
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(sdp.contains("m=audio 0 RTP/AVP 97\r\n"));
        assert!(sdp.contains("a=rtpmap:97 mpeg4-generic/44100/2\r\n"));
        // Every line ends the way SDP's do, not the way a Rust string does.
        assert!(sdp.ends_with("\r\n"));
        assert!(!sdp.contains("\n\n"));
    }

    #[test]
    fn each_track_is_named_by_the_id_its_units_carry() {
        let description = both();
        let sdp = describe(&description, "s");
        let controls: Vec<_> = sdp
            .lines()
            .filter_map(|line| line.strip_prefix("a=control:"))
            .collect();
        assert_eq!(controls, ["*", "trackID=0", "trackID=1"]);
        assert_eq!(description.tracks()[1].id().index(), 1);
    }

    #[test]
    fn the_parameter_sets_go_in_the_sdp_rather_than_the_stream() {
        let sdp = describe(&both(), "s");
        let fmtp = sdp
            .lines()
            .find(|line| line.starts_with("a=fmtp:96"))
            .unwrap();

        assert!(fmtp.contains("packetization-mode=1"));
        // The three bytes after the SPS header byte.
        assert!(fmtp.contains("profile-level-id=42c01e"), "{fmtp}");
        // Base64 of the SPS, then of the PPS, in that order.
        assert!(
            fmtp.contains("sprop-parameter-sets=Z0LAHtkAgA==,aM48gA=="),
            "{fmtp}"
        );
    }

    #[test]
    fn the_audio_configuration_goes_out_as_the_bytes_it_arrived_as() {
        let sdp = describe(&both(), "s");
        let fmtp = sdp
            .lines()
            .find(|line| line.starts_with("a=fmtp:97"))
            .unwrap();

        assert!(fmtp.contains("mode=AAC-hbr"));
        // The widths a receiver reads the AU headers with.
        assert!(fmtp.contains("sizelength=13"));
        assert!(fmtp.contains("indexlength=3"));
        assert!(fmtp.contains("indexdeltalength=3"));
        assert!(fmtp.contains("config=1210"), "{fmtp}");
    }

    #[test]
    fn a_configuration_this_did_not_fully_read_goes_out_whole() {
        // The bytes past the three fields this reads are an SBR extension.
        // A receiver needs them even though nothing here understands them.
        let description = Description::new(vec![Codec::Aac(
            aac::Parameters::parse(Bytes::from_static(&[0x12, 0x08, 0x56, 0xe5, 0x00])).unwrap(),
        )])
        .unwrap();
        let sdp = describe(&description, "s");
        assert!(sdp.contains("config=120856e500"), "{sdp}");
        assert!(sdp.contains("mpeg4-generic/44100/1"), "{sdp}");
    }

    #[test]
    fn a_stream_with_one_track_describes_one() {
        let description = Description::new(vec![Codec::H264(h264::Parameters {
            sps: vec![sps()],
            pps: vec![pps()],
        })])
        .unwrap();
        let sdp = describe(&description, "s");
        assert_eq!(sdp.matches("m=").count(), 1);
        assert!(sdp.contains("m=video"));
        assert!(!sdp.contains("m=audio"));
    }

    #[test]
    fn nowhere_in_this_says_where_to_send_anything() {
        // Which is settled by SETUP, per track, after the client has read
        // this. See the module docs.
        let sdp = describe(&both(), "s");
        assert!(sdp.contains("c=IN IP4 0.0.0.0"));
        assert_eq!(sdp.matches(" 0 RTP/AVP ").count(), 2);
    }

    #[test]
    fn a_name_cannot_end_the_field_it_is_in() {
        let sdp = describe(&both(), "live\r\na=control:evil");
        assert_eq!(attribute(&sdp, "control"), Some("*"));
        assert!(sdp.contains("s=live  a=control:evil\r\n"), "{sdp}");
    }

    #[test]
    fn base64_writes_what_the_standard_says() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Every bit pattern, so the alphabet is right end to end.
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn hex_is_two_digits_a_byte() {
        assert_eq!(hex(&[0x42, 0xc0, 0x1e]), "42c01e");
        assert_eq!(hex(&[0x00, 0x0f]), "000f");
    }
}
