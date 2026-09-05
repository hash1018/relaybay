//! What an RTSP connection means, with the socket left out.
//!
//! Requests go in and what to do about them comes back: an answer to write,
//! a stream to attach to, a session to end. As on the RTMP side it holds no
//! socket and waits for nothing, so a whole play can be driven in a test.
//!
//! # The exchange
//!
//! ```text
//! OPTIONS                       ──▶  the methods this answers
//! DESCRIBE rtsp://h/live/cam1   ──▶  the SDP, and the base its controls
//!                                    are relative to
//! SETUP    …/trackID=0          ──▶  a session id, and the transport agreed
//! SETUP    …/trackID=1          ──▶  the same session, a second track
//! PLAY     rtsp://h/live/cam1   ──▶  Play { path, tracks }, and packets
//!                                    start
//! TEARDOWN                      ──▶  Teardown
//! ```
//!
//! # Nothing here fails
//!
//! A request this cannot answer is answered with a status saying so, not
//! with an error that ends the connection. RTSP is a conversation: a client
//! that asks for a path which does not exist, or a transport this cannot
//! provide, is expected to be told and to ask for something else. Only bytes
//! that are not RTSP at all end a connection, and that is decided one layer
//! down in [`super::message`].
//!
//! # Why the session asks for a description rather than holding one
//!
//! `DESCRIBE` needs to know what a path is, and what a path is lives in a
//! registry full of channels and locks. Taking a lookup as an argument keeps
//! that on the far side of this module's edge: what arrives here is a
//! description or nothing, and the answer is the same either way.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use crate::rtsp::message::{Method, Request, Response, Status, Transport};
use crate::rtsp::sdp;
use crate::track::{Description, TrackId};

/// The methods this answers, as `OPTIONS` reports them.
const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER";

/// How long a client may go without a word before its session may be
/// dropped. Written into the `Session` header so a client knows to send
/// `GET_PARAMETER` if it has nothing else to say.
const TIMEOUT: u32 = 60;

/// Hands out session ids that no two connections share.
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// One track a client has asked for, and where it wants it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub track: TrackId,
    pub transport: Transport,
}

/// What the session wants done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write this answer.
    Respond(Box<Response>),

    /// Attach to this path and start sending the tracks that were set up.
    ///
    /// Follows the answer that tells the client the same thing, because a
    /// packet arriving before the response to `PLAY` is a packet for a
    /// stream the client has not been told has started.
    Play { path: String, tracks: Vec<Setup> },

    /// Stop sending, and keep the session.
    Pause,

    /// The session is over.
    Teardown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Nothing has been set up.
    Init,
    /// At least one track has, and the client has not said to start.
    Ready,
    /// Sending.
    Playing,
}

/// One RTSP connection's state.
pub struct Session {
    state: State,
    id: String,
    /// The path every request on this connection has to name, once one has.
    path: Option<String>,
    tracks: Vec<Setup>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// A connection that has said nothing yet.
    pub fn new() -> Self {
        let id = SESSIONS.fetch_add(1, Ordering::Relaxed);
        Self {
            state: State::Init,
            // Eight hex digits, spread out so that consecutive connections
            // do not get consecutive ids. Nothing depends on them being hard
            // to guess; a client only ever repeats the one it was given.
            id: format!("{:08x}", scramble(id) as u32),
            path: None,
            tracks: Vec::new(),
        }
    }

    /// The session id a client repeats on every request after `SETUP`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The path this connection is reading, once one has been named.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Takes one request and says what to do about it.
    ///
    /// `describe` is asked what a path is, and answers `None` for one
    /// nothing is publishing to. See the module docs on why it is an
    /// argument.
    pub fn handle(
        &mut self,
        request: &Request,
        describe: impl FnOnce(&str) -> Option<Arc<Description>>,
    ) -> Vec<Action> {
        let sequence = request.sequence().unwrap_or("0").to_owned();
        let answer = |status| Action::Respond(Box::new(reply(status, &sequence)));

        match request.method {
            Method::Options => vec![Action::Respond(Box::new(
                reply(Status::OK, &sequence).with("Public", PUBLIC),
            ))],
            Method::Describe => self.describe(request, &sequence, describe),
            Method::Setup => self.setup(request, &sequence, describe),
            Method::Play => self.play(&sequence),
            Method::Pause => self.pause(&sequence),
            Method::Teardown => vec![
                Action::Respond(Box::new(
                    reply(Status::OK, &sequence).with("Session", self.id.clone()),
                )),
                Action::Teardown,
            ],
            // A way of saying nothing, which is what a client sends to show
            // it is still there.
            Method::GetParameter => vec![Action::Respond(Box::new(
                reply(Status::OK, &sequence).with("Session", self.id.clone()),
            ))],
            // This serves streams; it does not take them. RTMP is how one
            // gets in.
            Method::Announce | Method::Record | Method::SetParameter | Method::Other(_) => {
                vec![answer(Status::METHOD_NOT_ALLOWED)]
            }
        }
    }

    fn describe(
        &mut self,
        request: &Request,
        sequence: &str,
        describe: impl FnOnce(&str) -> Option<Arc<Description>>,
    ) -> Vec<Action> {
        let path = path_of(&request.uri);
        let Some(description) = describe(&path) else {
            return vec![Action::Respond(Box::new(reply(
                Status::NOT_FOUND,
                sequence,
            )))];
        };
        self.path = Some(path.clone());

        // What the SDP's relative controls are resolved against. It has to
        // end in a slash, or a client appending `trackID=0` produces the
        // path with the last segment replaced rather than extended.
        let base = format!("{}/", request.uri.trim_end_matches('/'));
        vec![Action::Respond(Box::new(
            reply(Status::OK, sequence)
                .with("Content-Base", base)
                .with_body(
                    "application/sdp",
                    Bytes::from(sdp::describe(&description, &path)),
                ),
        ))]
    }

    fn setup(
        &mut self,
        request: &Request,
        sequence: &str,
        describe: impl FnOnce(&str) -> Option<Arc<Description>>,
    ) -> Vec<Action> {
        let answer = |status| vec![Action::Respond(Box::new(reply(status, sequence)))];
        if self.state == State::Playing {
            // Adding a track to a stream already running would leave the two
            // starting at different moments, and nothing asks for it.
            return answer(Status::METHOD_NOT_VALID);
        }

        let (path, index) = split_track(&path_of(&request.uri));
        // Every track of a session comes from one path. A client that named
        // two is confused about which stream it is watching.
        if self.path.as_ref().is_some_and(|first| *first != path) {
            return answer(Status::NOT_FOUND);
        }
        let Some(description) = describe(&path) else {
            return answer(Status::NOT_FOUND);
        };
        // Without a track in the URI there is nothing to set up: the client
        // read the SDP and every media section in it named one.
        let Some(track) = index
            .and_then(|index| description.tracks().get(index))
            .map(crate::track::Track::id)
        else {
            return answer(Status::NOT_FOUND);
        };

        let Some(transport) = request.headers.get("Transport").and_then(Transport::parse) else {
            return answer(Status::UNSUPPORTED_TRANSPORT);
        };

        self.path = Some(path);
        // A client that sets the same track up twice means the second one.
        self.tracks.retain(|setup| setup.track != track);
        self.tracks.push(Setup { track, transport });
        self.state = State::Ready;

        vec![Action::Respond(Box::new(
            reply(Status::OK, sequence)
                .with("Session", format!("{};timeout={TIMEOUT}", self.id))
                .with("Transport", transport.to_header(None)),
        ))]
    }

    fn play(&mut self, sequence: &str) -> Vec<Action> {
        let (State::Ready | State::Playing, Some(path)) = (self.state, self.path.clone()) else {
            return vec![Action::Respond(Box::new(reply(
                Status::METHOD_NOT_VALID,
                sequence,
            )))];
        };
        self.state = State::Playing;
        vec![
            Action::Respond(Box::new(
                reply(Status::OK, sequence).with("Session", self.id.clone()),
            )),
            // After the answer, never before: a packet for a stream the
            // client has not been told has started is a packet it discards.
            Action::Play {
                path,
                tracks: self.tracks.clone(),
            },
        ]
    }

    fn pause(&mut self, sequence: &str) -> Vec<Action> {
        if self.state != State::Playing {
            return vec![Action::Respond(Box::new(reply(
                Status::METHOD_NOT_VALID,
                sequence,
            )))];
        }
        self.state = State::Ready;
        vec![
            Action::Respond(Box::new(
                reply(Status::OK, sequence).with("Session", self.id.clone()),
            )),
            Action::Pause,
        ]
    }
}

/// An answer with the sequence number it answers, which every one carries.
fn reply(status: Status, sequence: &str) -> Response {
    Response::new(status).with("CSeq", sequence)
}

/// The path part of a request URI, without the scheme, the host or the
/// slashes around it.
///
/// `rtsp://host:8554/live/cam1/` becomes `live/cam1`. A URI that is not
/// absolute is taken as a path already, which is what `OPTIONS *` and a few
/// clients send.
fn path_of(uri: &str) -> String {
    let without_scheme = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let path = match uri.split_once("://") {
        // Past the authority, which ends at the first slash.
        Some(_) => without_scheme.split_once('/').map_or("", |(_, path)| path),
        None => uri,
    };
    // A query is a token or a hint, not part of the name — the same reason
    // RTMP's stream keys lose theirs.
    let path = path.split('?').next().unwrap_or(path);
    path.trim_matches('/').to_owned()
}

/// Splits a `SETUP` path into the stream and the track it names.
///
/// The last segment of a `SETUP` URI is the control the SDP gave that track.
/// Clients write it three ways and mean the same thing by all of them.
fn split_track(path: &str) -> (String, Option<usize>) {
    let Some((stream, last)) = path.rsplit_once('/') else {
        return (path.to_owned(), None);
    };
    let Some((name, index)) = last.split_once('=') else {
        return (path.to_owned(), None);
    };
    let names_a_track = ["trackid", "streamid", "track"]
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known));
    match index.parse() {
        Ok(index) if names_a_track => (stream.to_owned(), Some(index)),
        _ => (path.to_owned(), None),
    }
}

/// Spreads a counter's bits about, so that consecutive sessions do not get
/// consecutive ids. One step of splitmix64.
fn scramble(value: u64) -> u64 {
    let mut mixed = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{aac, h264};
    use crate::rtsp::message::Headers;
    use crate::track::Codec;

    fn description() -> Arc<Description> {
        Arc::new(
            Description::new(vec![
                Codec::H264(h264::Parameters {
                    sps: vec![
                        h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e])).unwrap(),
                    ],
                    pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce])).unwrap()],
                }),
                Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()),
            ])
            .unwrap(),
        )
    }

    fn known(path: &str) -> Option<Arc<Description>> {
        (path == "live/cam1").then(description)
    }

    fn nothing(_: &str) -> Option<Arc<Description>> {
        None
    }

    fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut all = Headers::new().with("CSeq", "7");
        for (name, value) in headers {
            all.push(name, *value);
        }
        Request {
            method,
            uri: uri.to_owned(),
            headers: all,
            body: Bytes::new(),
        }
    }

    fn response(actions: &[Action]) -> &Response {
        match actions.first() {
            Some(Action::Respond(response)) => response,
            other => panic!("an answer, got {other:?}"),
        }
    }

    fn interleaved(rtp: u8, rtcp: u8) -> String {
        format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp}")
    }

    /// A session that has described and set up both tracks.
    fn ready() -> Session {
        let mut session = Session::new();
        session.handle(&request(Method::Describe, "rtsp://h/live/cam1", &[]), known);
        for (index, channels) in [(0, interleaved(0, 1)), (1, interleaved(2, 3))] {
            session.handle(
                &request(
                    Method::Setup,
                    &format!("rtsp://h/live/cam1/trackID={index}"),
                    &[("Transport", &channels)],
                ),
                known,
            );
        }
        session
    }

    #[test]
    fn options_says_what_this_answers() {
        let actions = Session::new().handle(&request(Method::Options, "*", &[]), nothing);
        let response = response(&actions);
        assert_eq!(response.status, Status::OK);
        assert_eq!(response.headers.get("CSeq"), Some("7"));
        for method in ["DESCRIBE", "SETUP", "PLAY", "TEARDOWN"] {
            assert!(response.headers.get("Public").unwrap().contains(method));
        }
    }

    #[test]
    fn describe_answers_with_the_sdp_and_where_its_controls_point() {
        let mut session = Session::new();
        let actions = session.handle(
            &request(Method::Describe, "rtsp://h:8554/live/cam1", &[]),
            known,
        );
        let response = response(&actions);

        assert_eq!(response.status, Status::OK);
        assert_eq!(
            response.headers.get("Content-Type"),
            Some("application/sdp")
        );
        // The base has to end in a slash, or appending `trackID=0` replaces
        // the last segment instead of extending it.
        assert_eq!(
            response.headers.get("Content-Base"),
            Some("rtsp://h:8554/live/cam1/")
        );
        let sdp = String::from_utf8(response.body.to_vec()).unwrap();
        assert!(sdp.contains("m=video"));
        assert!(sdp.contains("a=control:trackID=1"));
        assert_eq!(session.path(), Some("live/cam1"));
    }

    #[test]
    fn describing_a_path_nothing_is_publishing_to_is_answered_not_found() {
        let actions = Session::new().handle(
            &request(Method::Describe, "rtsp://h/live/nobody", &[]),
            nothing,
        );
        assert_eq!(response(&actions).status, Status::NOT_FOUND);
    }

    #[test]
    fn setup_agrees_a_transport_and_names_the_session() {
        let mut session = Session::new();
        let actions = session.handle(
            &request(
                Method::Setup,
                "rtsp://h/live/cam1/trackID=0",
                &[("Transport", &interleaved(0, 1))],
            ),
            known,
        );
        let response = response(&actions);

        assert_eq!(response.status, Status::OK);
        assert_eq!(
            response.headers.get("Transport"),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
        // The id, and how long the client may go quiet.
        let named = response.headers.get("Session").unwrap();
        assert!(named.starts_with(session.id()), "{named}");
        assert!(named.contains("timeout=60"), "{named}");
    }

    #[test]
    fn the_ways_clients_write_a_track_all_name_the_same_one() {
        for control in ["trackID=1", "trackId=1", "streamid=1", "track=1"] {
            let mut session = Session::new();
            session.handle(
                &request(
                    Method::Setup,
                    &format!("rtsp://h/live/cam1/{control}"),
                    &[("Transport", &interleaved(0, 1))],
                ),
                known,
            );
            let actions = session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);
            let Some(Action::Play { tracks, .. }) = actions.get(1) else {
                panic!("{control}: a play")
            };
            assert_eq!(tracks[0].track.index(), 1, "{control}");
        }
    }

    #[test]
    fn a_track_the_stream_does_not_have_is_answered_not_found() {
        let mut session = Session::new();
        for uri in [
            "rtsp://h/live/cam1/trackID=9",
            // No track named at all: the SDP gave every media section one.
            "rtsp://h/live/cam1",
        ] {
            let actions = session.handle(
                &request(Method::Setup, uri, &[("Transport", &interleaved(0, 1))]),
                known,
            );
            assert_eq!(response(&actions).status, Status::NOT_FOUND, "{uri}");
        }
    }

    #[test]
    fn a_transport_this_cannot_provide_is_answered_as_such() {
        let mut session = Session::new();
        for header in ["RTP/AVP;multicast;port=5000-5001", "RAW/RAW/UDP;unicast"] {
            let actions = session.handle(
                &request(
                    Method::Setup,
                    "rtsp://h/live/cam1/trackID=0",
                    &[("Transport", header)],
                ),
                known,
            );
            assert_eq!(
                response(&actions).status,
                Status::UNSUPPORTED_TRANSPORT,
                "{header}"
            );
        }
    }

    #[test]
    fn every_track_of_a_session_has_to_come_from_one_stream() {
        let mut session = Session::new();
        session.handle(
            &request(
                Method::Setup,
                "rtsp://h/live/cam1/trackID=0",
                &[("Transport", &interleaved(0, 1))],
            ),
            known,
        );
        let actions = session.handle(
            &request(
                Method::Setup,
                "rtsp://h/live/other/trackID=0",
                &[("Transport", &interleaved(2, 3))],
            ),
            |path| (path == "live/other").then(description),
        );
        assert_eq!(response(&actions).status, Status::NOT_FOUND);
    }

    #[test]
    fn play_answers_first_and_starts_second() {
        let mut session = ready();
        let actions = session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);

        // A packet for a stream the client has not been told has started is
        // a packet it throws away.
        assert!(matches!(actions[0], Action::Respond(_)));
        let Action::Play { path, tracks } = &actions[1] else {
            panic!("a play, got {:?}", actions[1])
        };
        assert_eq!(path, "live/cam1");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track.index(), 0);
        assert_eq!(
            tracks[1].transport,
            Transport::Interleaved { rtp: 2, rtcp: 3 }
        );
        assert_eq!(
            response(&actions).headers.get("Session"),
            Some(session.id())
        );
    }

    #[test]
    fn setting_a_track_up_twice_keeps_the_second_answer() {
        let mut session = Session::new();
        for channels in [interleaved(0, 1), interleaved(4, 5)] {
            session.handle(
                &request(
                    Method::Setup,
                    "rtsp://h/live/cam1/trackID=0",
                    &[("Transport", &channels)],
                ),
                known,
            );
        }
        let actions = session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);
        let Some(Action::Play { tracks, .. }) = actions.get(1) else {
            panic!("a play")
        };
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].transport,
            Transport::Interleaved { rtp: 4, rtcp: 5 }
        );
    }

    #[test]
    fn playing_before_setting_anything_up_is_refused_without_ending_anything() {
        let mut session = Session::new();
        let actions = session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);
        assert_eq!(response(&actions).status, Status::METHOD_NOT_VALID);
        assert_eq!(actions.len(), 1, "the connection goes on");
    }

    #[test]
    fn pausing_stops_the_packets_and_playing_starts_them_again() {
        let mut session = ready();
        session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);

        let actions = session.handle(&request(Method::Pause, "rtsp://h/live/cam1", &[]), known);
        assert_eq!(response(&actions).status, Status::OK);
        assert_eq!(actions[1], Action::Pause);

        let actions = session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);
        assert!(matches!(actions[1], Action::Play { .. }));
    }

    #[test]
    fn setting_up_another_track_mid_stream_is_refused() {
        let mut session = ready();
        session.handle(&request(Method::Play, "rtsp://h/live/cam1", &[]), known);
        let actions = session.handle(
            &request(
                Method::Setup,
                "rtsp://h/live/cam1/trackID=1",
                &[("Transport", &interleaved(6, 7))],
            ),
            known,
        );
        assert_eq!(response(&actions).status, Status::METHOD_NOT_VALID);
    }

    #[test]
    fn teardown_answers_and_then_ends_it() {
        let mut session = ready();
        let actions = session.handle(&request(Method::Teardown, "rtsp://h/live/cam1", &[]), known);
        assert_eq!(response(&actions).status, Status::OK);
        assert_eq!(actions[1], Action::Teardown);
    }

    #[test]
    fn a_client_saying_nothing_is_answered_that_it_was_heard() {
        let mut session = ready();
        let actions = session.handle(
            &request(Method::GetParameter, "rtsp://h/live/cam1", &[]),
            known,
        );
        assert_eq!(response(&actions).status, Status::OK);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn a_client_trying_to_publish_is_turned_down() {
        // This serves streams and does not take them; RTMP is how one gets
        // in. Turned down rather than ignored, so the client stops asking.
        let mut session = Session::new();
        for method in [Method::Announce, Method::Record] {
            let actions = session.handle(&request(method, "rtsp://h/live/cam1", &[]), known);
            assert_eq!(response(&actions).status, Status::METHOD_NOT_ALLOWED);
        }
    }

    #[test]
    fn every_answer_carries_the_sequence_it_answers() {
        let mut session = ready();
        for method in [
            Method::Options,
            Method::Describe,
            Method::Play,
            Method::GetParameter,
            Method::Teardown,
        ] {
            let actions = session.handle(&request(method, "rtsp://h/live/cam1", &[]), known);
            assert_eq!(response(&actions).headers.get("CSeq"), Some("7"));
        }
    }

    #[test]
    fn a_uri_reads_as_the_path_inside_it() {
        assert_eq!(path_of("rtsp://host/live/cam1"), "live/cam1");
        assert_eq!(path_of("rtsp://host:8554/live/cam1/"), "live/cam1");
        assert_eq!(path_of("rtsp://host/live/cam1?token=abc"), "live/cam1");
        assert_eq!(path_of("/live/cam1"), "live/cam1");
        assert_eq!(path_of("rtsp://host"), "");
        assert_eq!(path_of("*"), "*");
    }

    #[test]
    fn a_last_segment_that_is_not_a_track_stays_part_of_the_path() {
        // A stream may legitimately be called anything, and a path segment
        // with an equals sign in it is not a control.
        assert_eq!(
            split_track("live/cam=1"),
            ("live/cam=1".to_owned(), None),
            "not a name this knows"
        );
        assert_eq!(
            split_track("live/trackID=x"),
            ("live/trackID=x".to_owned(), None),
            "not a number"
        );
        assert_eq!(
            split_track("live/cam1"),
            ("live/cam1".to_owned(), None),
            "no control at all"
        );
    }

    #[test]
    fn two_sessions_do_not_share_an_id() {
        assert_ne!(Session::new().id(), Session::new().id());
    }
}
